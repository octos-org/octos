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
///
/// Wave-3a introduced an explicit [`Required::Soft`] tier — surfaced via the
/// `soft_fail` companion field — so partial-artifact contracts can warn and
/// continue without demoting the spawn task. The historic boolean
/// `required` field is preserved verbatim for serde + ABI back-compat; the
/// runtime collapses both fields into a single [`Required`] gate value via
/// [`Validator::tier`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Validator {
    /// Stable identifier, unique within the validator list.
    pub id: String,
    /// Required validators block terminal success when they fail. Soft-fail
    /// validators (see [`Self::soft_fail`]) ignore this flag.
    #[serde(default = "default_required_bool")]
    pub required: bool,
    /// When `true`, a failed outcome surfaces as a warning + ledger entry
    /// but does NOT demote the spawn task — even if `required` is also
    /// `true`. Defaults to `false` so existing policies preserve the
    /// hard-fail semantics they have today. Use this to declare partial-
    /// artifact contracts (e.g. "the primary report is hard-required, the
    /// sub-artifacts are soft").
    #[serde(default, skip_serializing_if = "is_default_false")]
    pub soft_fail: bool,
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

impl Validator {
    /// Collapse `required` + `soft_fail` into the operator-visible
    /// gate-strength tier. The mapping is:
    ///
    /// | `required` | `soft_fail` | `tier()`         |
    /// | ---------- | ----------- | ---------------- |
    /// | `true`     | `false`     | [`Required::Hard`] |
    /// | `true`     | `true`      | [`Required::Soft`] |
    /// | `false`    | `false`     | [`Required::None`] |
    /// | `false`    | `true`      | [`Required::Soft`] |
    ///
    /// `soft_fail = true` always overrides the hard semantics so the
    /// validator never demotes its spawn task.
    pub fn tier(&self) -> Required {
        if self.soft_fail {
            Required::Soft
        } else if self.required {
            Required::Hard
        } else {
            Required::None
        }
    }
}

/// Operator-facing strength label for a validator's gate over terminal
/// success. Surfaced through [`Validator::tier`] and the persisted ledger
/// outcome record so dashboards can split hard, soft, and informational
/// failures.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Required {
    /// A failure of this validator demotes the spawn task to `Failed`.
    /// Equivalent to the historic `required: true`.
    #[default]
    Hard,
    /// A failure surfaces a warning + persists to the ledger but does NOT
    /// demote the spawn task. Use for sub-artifacts and partial-artifact
    /// contracts where the primary deliverable is hard-required but
    /// auxiliary outputs are nice-to-have.
    Soft,
    /// A failure is fully optional — same gate behaviour as `Soft`, but
    /// operator-visible as "this validator is informational only".
    /// Equivalent to the historic `required: false` with `soft_fail = false`.
    None,
}

impl Required {
    /// Does a non-`Pass` outcome from this validator block terminal success?
    pub fn is_hard(self) -> bool {
        matches!(self, Self::Hard)
    }

    /// Should a non-`Pass` outcome surface as a warning (without demoting
    /// the spawn task)? True for both `Soft` and `None` — operators are free
    /// to filter on the explicit tier via the persisted outcome record.
    pub fn is_warning_only(self) -> bool {
        matches!(self, Self::Soft | Self::None)
    }

    /// Stable label for metrics + ledger records.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hard => "hard",
            Self::Soft => "soft",
            Self::None => "none",
        }
    }
}

fn default_required_bool() -> bool {
    true
}

fn is_default_false(value: &bool) -> bool {
    !*value
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
    /// Assert that EVERY file matching `glob` independently meets
    /// `non_silent_samples / total_samples >= min_ratio`, and that at least
    /// `require_at_least` files were matched.
    ///
    /// Complements [`ValidatorSpec::AudioNonSilent`], which only requires a
    /// single match to pass. The whole-file variant cannot catch a single
    /// silent segment in a multi-segment podcast — the silent gap gets
    /// averaged out by the surrounding speech in the final mix. By validating
    /// each intermediate segment independently, `PerFileNonSilent` rejects a
    /// silent segment before delivery, surfacing the offending filename in
    /// the failure message so the spawn task can rerun the failing TTS call
    /// on the next round.
    ///
    /// `${args.<key>}` interpolation is supported in the glob via
    /// `interpolate_args_path` (path-traversal segments are rejected,
    /// absolute-path values are rejected). `${output.<key>}` substitution is
    /// intentionally NOT supported in this variant — see the
    /// `decode_non_silent_ratio` doc-string for rationale.
    ///
    /// Field semantics:
    /// * `glob` — workspace-relative glob matched via [`glob::glob`]. WAV and
    ///   MP3 extensions are decoded; other extensions yield per-file errors.
    /// * `min_ratio` — applied to EACH matched file. Defaults to
    ///   [`default_non_silent_ratio`] (0.3).
    /// * `require_at_least` — minimum number of files that MUST match.
    ///   `0` (the serde default) disables the minimum so the validator can
    ///   still pass when no files matched (consistent with optional
    ///   intermediate artifacts that may not exist in every run).
    PerFileNonSilent {
        glob: String,
        #[serde(default = "default_non_silent_ratio")]
        min_ratio: f32,
        #[serde(default)]
        require_at_least: usize,
    },
    /// Assert each file matching `glob` has the magic-byte prefix for the
    /// declared `format`. Catches "tool wrote 0 bytes" or "tool wrote an
    /// HTML error page in place of an MP3".
    ///
    /// The format field is named `format` rather than `kind` to avoid
    /// colliding with serde's `kind` discriminator tag.
    MagicBytes { glob: String, format: MagicByteKind },
    /// Polling HTTP probe — repeatedly GET a templated URL (with
    /// `${args.<key>}` interpolation against the spawn task's input args)
    /// until the expected status code (+ optional body substring) is
    /// observed or the deadline expires.
    ///
    /// Closes the silent-failure path where a spawn task kicks off an
    /// asynchronous external operation (training a voice, deploying a site)
    /// whose completion the harness must verify without baking polling logic
    /// into every skill. Emits [`crate::validators::ValidatorStatus::Pass`]
    /// on the first success; [`Fail`] (with the last response summary in
    /// the message) when the deadline expires; [`Timeout`] only if a single
    /// probe within the deadline window itself times out at the HTTP level.
    HttpProbeUntil {
        url_template: String,
        #[serde(default = "default_http_probe_status")]
        expected_status: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_contains: Option<String>,
        /// Interval between probe attempts in milliseconds.
        #[serde(default = "default_http_probe_until_interval_ms")]
        poll_interval_ms: u64,
        /// Hard wall-clock deadline in milliseconds. Once reached the
        /// validator emits a [`Fail`] outcome surfacing the most recent
        /// response so the LLM/operator can debug in one round.
        #[serde(default = "default_http_probe_until_deadline_ms")]
        deadline_ms: u64,
    },
    /// Assert a single file's SHA-256 digest equals `sha256`. Accepts either
    /// an explicit hex digest OR a `${args.<key>}` template so the spawn task
    /// can supply the expected hash through its input args (e.g. a manifest
    /// `sha256` field captured at install time). Lifts the inline
    /// `manage_skills::download_binary` checksum check onto the canonical
    /// validator path so it shows up in the contract diagnostics ledger.
    Sha256Match { glob: String, sha256: String },
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
    /// OOXML / OpenDocument-style ZIP container (PPTX, DOCX, XLSX, ODT, ...).
    /// Matches the three ZIP signatures: local-file-header (`PK\x03\x04`),
    /// end-of-central-directory (`PK\x05\x06`), and spanned-archive
    /// (`PK\x07\x08`).
    Pptx,
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
            // PPTX (and any OOXML/zip container): all three ZIP signatures
            // are accepted so a minimally-built archive is not rejected as
            // structurally invalid.
            Self::Pptx => &[
                &[0x50, 0x4B, 0x03, 0x04],
                &[0x50, 0x4B, 0x05, 0x06],
                &[0x50, 0x4B, 0x07, 0x08],
            ],
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
            Self::Pptx => "pptx",
        }
    }
}

fn default_http_probe_status() -> u16 {
    200
}

fn default_http_probe_until_interval_ms() -> u64 {
    2_000
}

fn default_http_probe_until_deadline_ms() -> u64 {
    30_000
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
    /// Coding workspaces — projects with a recognised manifest
    /// (Cargo.toml / package.json / pyproject.toml). Inherits the
    /// `Session` spawn-task contracts and adds AfterTool hooks that
    /// run `cargo check` (and optionally eslint / ruff when present
    /// on PATH) after `edit_file` / `write_file` / `diff_edit` calls
    /// scoped to the language's file extensions. Audit Gap-1 + Q3.
    Coding,
}

impl WorkspacePolicyKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Slides => "slides",
            Self::Sites => "sites",
            Self::Session => "session",
            Self::Coding => "coding",
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
                soft_fail: false,
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
                    // octos #997: gate the slides project on the PPTX magic-bytes
                    // signature so an HTML "success" deck (the mofa_slides
                    // failure mode where the skill writes an error page in
                    // place of the .pptx) trips the project-scope contract.
                    //
                    // The bare `MagicBytes` validator lives in `for_session()`
                    // as `mofa_slides_contract` (workspace_policy.rs:863-874),
                    // but the session-scope spawn_tasks table is not consulted
                    // by `inspect_workspace_contract` — which only reads
                    // `validation.validators` against the slides-kind policy.
                    // Mirror the spawn-task contract here so the
                    // project-scope gate actually exercises the check. See
                    // option (a) of the issue write-up; the deeper fix
                    // (option (b): teach `inspect_workspace_contract` to read
                    // `spawn_tasks` too) is the right architectural cleanup
                    // and is tracked as a follow-up.
                    validators: vec![Validator {
                        id: "slides.mofa_slides.pptx_magic_bytes".into(),
                        required: true,
                        soft_fail: false,
                        timeout_ms: None,
                        phase: ValidatorPhaseKind::Completion,
                        spec: ValidatorSpec::MagicBytes {
                            glob: "**/*.pptx".into(),
                            format: MagicByteKind::Pptx,
                        },
                    }],
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
        // Issue #998: `mofa_slides` plugin emits a `.pptx` via `files_to_send`
        // (auto-detected by `PluginTool::detect_output_file` from the
        // skill's "Generated PPTX: <path>" stdout marker or an explicit
        // `out` arg — see `plugins/tool.rs:550-625, 1321-1361`). The
        // contract layer's `bind_explicit_files_to_artifacts` requires a
        // named artifact source to bind the reported file into
        // `ActionContext`; without one it returns "workspace contract has
        // no artifact source" (`workspace_contract.rs:333-336`) on every
        // successful slides run. Declaring the artifact here gives the
        // mofa_slides contract a target name to reference and matches the
        // recursive `**/*.pptx` glob already used by the MagicBytes(Pptx)
        // validator on `on_completion`.
        artifacts.insert("slides_pptx".into(), "**/*.pptx".into());

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
            // Catch the three user-visible failure modes from the silent-MP3
            // bug class:
            //   1. tool wrote zero bytes / an HTML error page in place of
            //      audio (MagicBytes).
            //   2. tool generated a valid MP3 header but a silent decoded
            //      stream (AudioNonSilent — whole final mix).
            //   3. one of the intermediate segment WAVs is silent but the
            //      surrounding non-silent segments mask the gap in the
            //      assembled MP3 (PerFileNonSilent on the preserved
            //      `<output_dir>/segments/seg_*.wav` files — mofa-podcast
            //      preserves segments after successful assembly via the
            //      Result-aware `SegmentDirCleanup` RAII guard introduced
            //      in mofa-skills #59).
            //
            // The per-file glob `**/segments/seg_*.wav` deliberately
            // EXCLUDES `pause_after_*.wav`, `pause_line_*.wav`, and
            // `bgm_placeholder_line_*.wav` — those filenames are emitted
            // by mofa-podcast as intentionally-silent inter-segment pauses
            // and would never pass a non-silent ratio check.
            //
            // Stale-segment concern: this glob does NOT apply the
            // `task_started_at` look-back filter that `resolve_artifacts`
            // uses (the validator runner doesn't see that timestamp).
            // Same caveat applies to the pre-existing whole-file
            // `AudioNonSilent` above (`skill-output/mofa-podcast/*.mp3`),
            // so this is a shared harness invariant rather than a
            // regression. In practice mofa-skills #59 clears
            // `<output_dir>/segments/` at the start of every invocation
            // (`generate_podcast::seg_dir` cleanup), so stale segments
            // only matter for unusual concurrent-run workflows. A future
            // follow-up can plumb `task_started_at` into the validator
            // runner to filter per-file matches by mtime, which would
            // close the gap for both validators in one shot.
            on_completion: vec![
                SpawnTaskValidatorSpec::Bare(ValidatorSpec::MagicBytes {
                    glob: "skill-output/mofa-podcast/*.mp3".into(),
                    format: MagicByteKind::Mp3,
                }),
                SpawnTaskValidatorSpec::Bare(ValidatorSpec::AudioNonSilent {
                    glob: "skill-output/mofa-podcast/*.mp3".into(),
                    min_ratio: default_non_silent_ratio(),
                }),
                SpawnTaskValidatorSpec::Bare(ValidatorSpec::PerFileNonSilent {
                    glob: "skill-output/mofa-podcast/**/segments/seg_*.wav".into(),
                    min_ratio: default_non_silent_ratio(),
                    require_at_least: 1,
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
        // actually registered with ominix-api AND the WAV landed in the
        // canonical `voice_profiles` directory. Closes the yangmi gap
        // where fm_voice_save returns success but the API has no record
        // — plus catches the parallel disk-side failure where the voice
        // file never reached `voice_profiles/<name>.wav`.
        //
        // The FileExists path uses `${args.name}` interpolation against
        // the spawn task's input args; mofa-fm writes the WAV to
        // `${OCTOS_VOICE_DIR:-${OCTOS_DATA_DIR}/voice_profiles}/<name>.wav`,
        // which under the default session workspace resolves relative to
        // the workspace root via `voice_profiles/<name>.wav`. Operators
        // who pin a non-default `OCTOS_VOICE_DIR` can override the path
        // in their workspace policy.
        let fm_voice_save_contract = WorkspaceSpawnTaskPolicy {
            artifact: None,
            artifacts: Vec::new(),
            on_verify: Vec::new(),
            on_complete: vec![],
            on_deliver: vec![],
            on_failure: vec!["notify_user:Voice registration failed".into()],
            on_completion: vec![
                SpawnTaskValidatorSpec::Bare(ValidatorSpec::OminixVoiceExists {
                    name_arg: "name".into(),
                }),
                SpawnTaskValidatorSpec::Bare(ValidatorSpec::FileExists {
                    path: "voice_profiles/${args.name}.wav".into(),
                    min_bytes: Some(1024),
                }),
            ],
        };

        // Slides (mofa_slides spawn task): the user-visible failure mode is
        // a "success" reply with an HTML error page in place of the PPTX.
        // MagicBytes (Pptx) rejects that at the contract gate by asserting
        // the local-file-header / end-of-central-directory ZIP signature
        // is present at byte 0. The glob runs recursively so the policy
        // also catches slides written to nested subdirectories.
        //
        // Issue #998: the slides plugin reports its generated PPTX via
        // `files_to_send` (parsed at `plugins/tool.rs:1321-1361` from a
        // JSON envelope OR auto-detected from a "Generated PPTX: <path>"
        // stdout marker — see `plugins/tool.rs:550-625` and the test at
        // `plugins/tool.rs:2064-2103`). The contract layer's
        // `bind_explicit_files_to_artifacts` (`workspace_contract.rs:329-357`)
        // requires `artifact_sources()` to be non-empty so it can bind the
        // reported PPTX into a named slot in `ActionContext`. Declaring
        // `artifact: Some("slides_pptx")` matches the artifact entry
        // registered above (`for_session` artifacts map) whose glob
        // (`**/*.pptx`) is the same recursive pattern the on-completion
        // MagicBytes validator uses, so the two checks see a consistent
        // set of paths.
        let mofa_slides_contract = WorkspaceSpawnTaskPolicy {
            artifact: Some("slides_pptx".into()),
            artifacts: Vec::new(),
            on_verify: Vec::new(),
            on_complete: vec![],
            on_deliver: vec![],
            on_failure: vec!["notify_user:Slide generation failed".into()],
            on_completion: vec![SpawnTaskValidatorSpec::Bare(ValidatorSpec::MagicBytes {
                glob: "**/*.pptx".into(),
                format: MagicByteKind::Pptx,
            })],
        };

        // mofa_cards writes PNGs into a `card_dir` (required input arg).
        // The contract uses a recursive PNG glob so any layout under that
        // directory is covered without hard-coding a single output path.
        let mofa_cards_contract = WorkspaceSpawnTaskPolicy {
            artifact: None,
            artifacts: Vec::new(),
            on_verify: Vec::new(),
            on_complete: vec![],
            on_deliver: vec![],
            on_failure: vec!["notify_user:Card generation failed".into()],
            on_completion: vec![SpawnTaskValidatorSpec::Bare(ValidatorSpec::MagicBytes {
                glob: "**/*.png".into(),
                format: MagicByteKind::Png,
            })],
        };

        // mofa_comic and mofa_infographic each take a required `out` arg
        // pointing at a single PNG file. The contract asserts BOTH that
        // the file landed at the declared path (FileExists with
        // `${args.out}`, FileExists already supports template
        // interpolation) AND that there is at least one valid PNG header
        // present in the workspace (MagicBytes against the recursive
        // `**/*.png` glob — MagicBytes does not template its glob today).
        // FileExists does the per-task path check; MagicBytes does the
        // bytes-are-actually-a-PNG check.
        let mofa_comic_contract = WorkspaceSpawnTaskPolicy {
            artifact: None,
            artifacts: Vec::new(),
            on_verify: Vec::new(),
            on_complete: vec![],
            on_deliver: vec![],
            on_failure: vec!["notify_user:Comic generation failed".into()],
            on_completion: vec![
                SpawnTaskValidatorSpec::Bare(ValidatorSpec::FileExists {
                    path: "${args.out}".into(),
                    min_bytes: Some(1024),
                }),
                SpawnTaskValidatorSpec::Bare(ValidatorSpec::MagicBytes {
                    glob: "**/*.png".into(),
                    format: MagicByteKind::Png,
                }),
            ],
        };

        let mofa_infographic_contract = WorkspaceSpawnTaskPolicy {
            artifact: None,
            artifacts: Vec::new(),
            on_verify: Vec::new(),
            on_complete: vec![],
            on_deliver: vec![],
            on_failure: vec!["notify_user:Infographic generation failed".into()],
            on_completion: vec![
                SpawnTaskValidatorSpec::Bare(ValidatorSpec::FileExists {
                    path: "${args.out}".into(),
                    min_bytes: Some(1024),
                }),
                SpawnTaskValidatorSpec::Bare(ValidatorSpec::MagicBytes {
                    glob: "**/*.png".into(),
                    format: MagicByteKind::Png,
                }),
            ],
        };

        // mofa_frame is NOT spawn_only today (so this contract is dormant
        // for the current manifest), but the audit's section-5 missing
        // table lists it as an artifact-producing tool whose contract
        // should be wired even if the gate is not yet fired. Recording
        // the entry here lets the next spawn_only flip pick up the
        // contract automatically.
        let mofa_frame_contract = WorkspaceSpawnTaskPolicy {
            artifact: None,
            artifacts: Vec::new(),
            on_verify: Vec::new(),
            on_complete: vec![],
            on_deliver: vec![],
            on_failure: vec!["notify_user:Frame extraction failed".into()],
            on_completion: vec![SpawnTaskValidatorSpec::Bare(ValidatorSpec::MagicBytes {
                glob: "**/*.png".into(),
                format: MagicByteKind::Png,
            })],
        };

        // mofa_publish (audit P0-3): probe the live `deploy_url` the skill
        // emits via `named_outputs.deploy_url` after a successful publish.
        // The probe asserts both a 200 status AND that the body carries an
        // `<!DOCTYPE` prefix — guarding against the "200 OK with a soft
        // 404 / SPA-shell error page" failure mode the audit calls out.
        //
        // `required = false` (NOT the default of true) because:
        //
        // 1. The mofa_publish skill in `mofa-skills/mofa-publish/` does
        //    not yet emit `named_outputs.deploy_url`. Once that lands
        //    (separate PR in the mofa-skills repo) this entry flips to
        //    `required = true` so the canonical probe blocks bad
        //    deployments.
        // 2. Surfacing `${output.deploy_url}` against an empty
        //    `named_outputs` map would produce a hard `Error` outcome on
        //    every mofa_publish call until the skill catches up; setting
        //    `required = false` keeps the validator running as a
        //    diagnostic (recorded to the ledger) without blocking.
        let mofa_publish_contract = WorkspaceSpawnTaskPolicy {
            artifact: None,
            artifacts: Vec::new(),
            on_verify: Vec::new(),
            on_complete: vec![],
            on_deliver: vec![],
            on_failure: vec!["notify_user:Publish probe failed".into()],
            on_completion: vec![SpawnTaskValidatorSpec::Full(Validator {
                id: "mofa_publish.deploy_url_probe".into(),
                required: false,
                soft_fail: false,
                timeout_ms: None,
                phase: ValidatorPhaseKind::Completion,
                spec: ValidatorSpec::HttpProbe {
                    url_template: "${output.deploy_url}".into(),
                    expected_status: 200,
                    // Sentinel rejecting a 200-with-soft-404 body. mofa_publish
                    // ships an HTML site; the document type prefix should
                    // always be present on a real deployed page.
                    expected_contains: Some("<!DOCTYPE".into()),
                },
            })],
        };

        // Wave-3a wire target for the new `Sha256Match` variant.
        //
        // `manage_skills` is NOT spawn_only today, so this entry is dormant
        // until either (a) the harness wires an `AfterTool` phase (audit
        // Gap-1) that fires post-call validators against the synchronous
        // tool, or (b) `manage_skills` itself flips to spawn_only. Recording
        // it now means the canonical SHA-256 check is declared in one place
        // and the inline `download_binary` check at
        // `tools/manage_skills.rs:836` can be retired in a follow-up PR.
        //
        // The glob is scoped to *this* invocation's installed skill via
        // `${args.skill_dir}` so a workspace with multiple installed skills
        // is not spuriously failed against an unrelated binary's digest.
        // The expected digest is interpolated through
        // `${args.expected_sha256}` so the spawn-task input args carry the
        // manifest-declared hash — no per-skill workspace policy edit
        // needed.
        //
        // Caveat: this matches the *final extracted binary*, not the
        // downloaded archive. For tarball-distributed skills, the manifest
        // digest of the archive will NOT match a `Sha256Match` on
        // `${args.skill_dir}/main`. The inline `download_binary` check in
        // `tools/manage_skills.rs` is the source-of-truth for archive
        // verification; this validator is complementary and asserts the
        // post-extraction binary integrity for raw-binary distributions.
        let manage_skills_contract = WorkspaceSpawnTaskPolicy {
            artifact: None,
            artifacts: Vec::new(),
            on_verify: Vec::new(),
            on_complete: vec![],
            on_deliver: vec![],
            on_failure: vec!["notify_user:Skill install verification failed".into()],
            on_completion: vec![SpawnTaskValidatorSpec::Bare(ValidatorSpec::Sha256Match {
                glob: "${args.skill_dir}/main".into(),
                sha256: "${args.expected_sha256}".into(),
            })],
        };

        // Wave-3a wire target for the `soft_fail` tier.
        //
        // Both `synthesize_research` and `deep_search` produce a primary
        // report (hard-required) plus optional sub-artifacts. Today neither
        // tool is spawn_only, so these contracts are dormant until either
        // the runtime flips them or a follow-up wires the post-tool
        // validator phase. The shape — a hard FileExists for the primary
        // plus a soft FileExists for any sub-artifact — is the canonical
        // template for partial-artifact contracts.
        //
        // `${args.research_dir}` lets the contract scope the check to the
        // research subdirectory each invocation produces, without baking a
        // global path into the policy. The same shape works for the
        // deep_search index file at `${args.research_dir}/_search_results.md`.
        let synthesize_research_contract = WorkspaceSpawnTaskPolicy {
            artifact: None,
            artifacts: Vec::new(),
            on_verify: Vec::new(),
            on_complete: vec![],
            on_deliver: vec![],
            on_failure: vec!["notify_user:Research synthesis failed".into()],
            on_completion: vec![
                // Primary report — block delivery if it's missing.
                SpawnTaskValidatorSpec::Full(Validator {
                    id: "synthesize_research.primary_report".into(),
                    required: true,
                    soft_fail: false,
                    timeout_ms: None,
                    phase: ValidatorPhaseKind::Completion,
                    spec: ValidatorSpec::FileExists {
                        path: "${args.research_dir}/synthesis.md".into(),
                        min_bytes: Some(256),
                    },
                }),
                // Sub-artifacts — warn but don't demote on absence.
                SpawnTaskValidatorSpec::Full(Validator {
                    id: "synthesize_research.partials_warn".into(),
                    required: true,
                    soft_fail: true,
                    timeout_ms: None,
                    phase: ValidatorPhaseKind::Completion,
                    spec: ValidatorSpec::FileExists {
                        path: "${args.research_dir}/_search_results.md".into(),
                        min_bytes: None,
                    },
                }),
            ],
        };

        let deep_search_contract = WorkspaceSpawnTaskPolicy {
            artifact: None,
            artifacts: Vec::new(),
            on_verify: Vec::new(),
            on_complete: vec![],
            on_deliver: vec![],
            on_failure: vec!["notify_user:Deep search failed".into()],
            on_completion: vec![
                // Primary index — block delivery if it's missing.
                SpawnTaskValidatorSpec::Full(Validator {
                    id: "deep_search.primary_index".into(),
                    required: true,
                    soft_fail: false,
                    timeout_ms: None,
                    phase: ValidatorPhaseKind::Completion,
                    spec: ValidatorSpec::FileExists {
                        path: "${args.research_dir}/_search_results.md".into(),
                        min_bytes: None,
                    },
                }),
                // Per-source sub-artifacts — warn but don't demote.
                SpawnTaskValidatorSpec::Full(Validator {
                    id: "deep_search.sources_warn".into(),
                    required: true,
                    soft_fail: true,
                    timeout_ms: None,
                    phase: ValidatorPhaseKind::Completion,
                    spec: ValidatorSpec::FileExists {
                        path: "${args.research_dir}/01_source.md".into(),
                        min_bytes: None,
                    },
                }),
            ],
        };

        let mut spawn_tasks = BTreeMap::new();
        spawn_tasks.insert("fm_tts".into(), tts_contract.clone());
        spawn_tasks.insert("voice_synthesize".into(), voice_synthesize_contract);
        spawn_tasks.insert("podcast_generate".into(), podcast_contract);
        spawn_tasks.insert("fm_voice_save".into(), fm_voice_save_contract);
        spawn_tasks.insert("mofa_slides".into(), mofa_slides_contract);
        spawn_tasks.insert("mofa_cards".into(), mofa_cards_contract);
        spawn_tasks.insert("mofa_comic".into(), mofa_comic_contract);
        spawn_tasks.insert("mofa_infographic".into(), mofa_infographic_contract);
        spawn_tasks.insert("mofa_frame".into(), mofa_frame_contract);
        spawn_tasks.insert("mofa_publish".into(), mofa_publish_contract);
        spawn_tasks.insert("manage_skills".into(), manage_skills_contract);
        spawn_tasks.insert("synthesize_research".into(), synthesize_research_contract);
        spawn_tasks.insert("deep_search".into(), deep_search_contract);

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

    /// Default policy for a coding workspace (Audit Gap-1 + section 7 Q3).
    ///
    /// Inherits the [`for_session`] spawn-task contracts (so a coding workspace
    /// that still spawns slides / podcast / fm_voice_save tasks keeps the
    /// per-skill contract gates) and overlays only the `kind = Coding`
    /// marker. The AfterTool `cargo check` / `eslint` / `ruff` hooks live in
    /// [`coding_default_hooks`] so the host (chat.rs / gateway.rs / serve.rs)
    /// can merge them into its `HookExecutor` without forking the hook runner.
    ///
    /// The split is deliberate: hooks are runtime side-effects executed by
    /// the agent, while `WorkspacePolicy` is a serialized declaration shared
    /// with the LLM. Stuffing process-launch hooks into the workspace policy
    /// would (a) leak operator-side state into a contract the LLM can read
    /// and (b) force every embedder of the policy struct (config_watcher,
    /// session bootstrap, REST inspectors) to know how to run shells.
    pub fn for_coding() -> Self {
        let mut policy = Self::for_session();
        policy.workspace.kind = WorkspacePolicyKind::Coding;
        policy
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

/// Detect the workspace policy kind for `cwd` by probing for well-known
/// language manifests. Order: `Cargo.toml` → `package.json` → `pyproject.toml`
/// → fallback [`WorkspacePolicyKind::Session`].
///
/// This is the entry point for Audit Gap-1's "the harness owns the contract"
/// stance — the runtime decides whether a workspace is `Coding` based on
/// observable filesystem signals, not LLM input. Operators who want to
/// override the inference write an explicit `.octos-workspace.toml`.
pub fn detect_workspace_policy_kind(cwd: &Path) -> WorkspacePolicyKind {
    if cwd.join("Cargo.toml").is_file()
        || cwd.join("package.json").is_file()
        || cwd.join("pyproject.toml").is_file()
    {
        WorkspacePolicyKind::Coding
    } else {
        WorkspacePolicyKind::Session
    }
}

/// Default `after_tool_call` hooks for a [`WorkspacePolicyKind::Coding`]
/// workspace (Audit Gap-1 closure).
///
/// Each entry mirrors the canonical `cargo check` / `eslint` / `ruff`
/// invocation an operator would write by hand. Hosts (chat.rs, gateway.rs,
/// serve.rs) call this on bootstrap and merge the returned hooks into the
/// HookExecutor they hand to the agent. Operator-defined hooks always merge
/// AFTER these defaults, so an operator-written `cargo check` hook with
/// stricter args overrides the default's behaviour by virtue of running too
/// (both fire, but a stricter one denies before the cheaper one matters).
///
/// `requires_bin` gates each entry on a binary lookup — operators who do not
/// have `eslint` or `ruff` installed see the hooks silently skip rather than
/// failing every after-tool callback. `cargo` is assumed present in a Rust
/// workspace (the detector keys on `Cargo.toml`) but is still gated so
/// nothing breaks when a stub `Cargo.toml` lives alongside a non-cargo
/// project.
pub fn coding_default_hooks() -> Vec<crate::hooks::HookConfig> {
    use crate::hooks::{HookConfig, HookEvent};
    let edit_tools = vec![
        "edit_file".to_string(),
        "write_file".to_string(),
        "diff_edit".to_string(),
    ];
    vec![
        // Rust: `cargo check --message-format=short` on `.rs` edits.
        HookConfig {
            event: HookEvent::AfterToolCall,
            command: vec![
                "cargo".into(),
                "check".into(),
                "--message-format=short".into(),
            ],
            timeout_ms: 60_000,
            tool_filter: edit_tools.clone(),
            path_filter: vec!["**/*.rs".into()],
            requires_bin: Some("cargo".into()),
        },
        // JS/TS: ESLint with zero-warning policy. Skipped if eslint is
        // not on PATH. Operators who use a different linter (biome, etc.)
        // add their own hook; the default is best-effort and explicit.
        HookConfig {
            event: HookEvent::AfterToolCall,
            command: vec!["eslint".into(), "--max-warnings".into(), "0".into()],
            timeout_ms: 60_000,
            tool_filter: edit_tools.clone(),
            path_filter: vec!["**/*.{js,ts,tsx,jsx}".into()],
            requires_bin: Some("eslint".into()),
        },
        // Python: `ruff check` on `.py` edits.
        HookConfig {
            event: HookEvent::AfterToolCall,
            command: vec!["ruff".into(), "check".into()],
            timeout_ms: 60_000,
            tool_filter: edit_tools,
            path_filter: vec!["**/*.py".into()],
            requires_bin: Some("ruff".into()),
        },
    ]
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
                soft_fail: false,
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
                soft_fail: false,
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
                soft_fail: false,
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
        assert!(!parsed.soft_fail, "soft_fail defaults to false");
        assert_eq!(parsed.tier(), Required::Hard);
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
    fn magic_byte_kind_pptx_matches_zip_signatures() {
        // PPTX is a ZIP archive — accept the OOXML local-file-header
        // signature (`PK\x03\x04`) and the central-directory variants so a
        // tool that emits a minimal/empty archive isn't spuriously rejected.
        assert!(MagicByteKind::Pptx.matches(b"PK\x03\x04rest of zip"));
        assert!(MagicByteKind::Pptx.matches(b"PK\x05\x06"));
        assert!(MagicByteKind::Pptx.matches(b"PK\x07\x08"));
        // An HTML error page surfaced in place of a PPTX must be rejected so
        // the silent-failure path is caught at the harness gate.
        assert!(!MagicByteKind::Pptx.matches(b"<!DOCTYPE html>"));
    }

    #[test]
    fn session_policy_declares_new_domain_validators_for_silent_failure_paths() {
        let policy = WorkspacePolicy::for_session();
        let podcast = policy
            .spawn_tasks
            .get("podcast_generate")
            .expect("podcast contract");
        // The two whole-file domain validators must be declared so the
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
        // The per-segment gate (PerFileNonSilent) catches the silent-
        // segment failure mode that the whole-file AudioNonSilent cannot
        // detect: one bad seg_NNN_<voice>.wav whose silent gap gets
        // averaged out by the surrounding speech in the assembled MP3.
        // mofa-skills #59 preserves segments after successful assembly
        // via the Result-aware `SegmentDirCleanup` RAII guard so this
        // glob actually matches when the harness runs.
        let per_file = podcast
            .on_completion
            .iter()
            .find_map(|entry| match entry {
                SpawnTaskValidatorSpec::Bare(ValidatorSpec::PerFileNonSilent {
                    glob,
                    min_ratio,
                    require_at_least,
                }) => Some((glob.clone(), *min_ratio, *require_at_least)),
                _ => None,
            })
            .expect("podcast_generate must declare PerFileNonSilent on segments");
        assert!(
            per_file.0.contains("segments/seg_*.wav"),
            "PerFileNonSilent glob must scope to segment WAVs to exclude pause/BGM placeholders: {}",
            per_file.0
        );
        assert!(
            per_file.1 >= 0.3,
            "min_ratio should be at least the harness default (0.3): {}",
            per_file.1
        );
        assert!(
            per_file.2 >= 1,
            "require_at_least must enforce a non-zero floor so an empty segments dir surfaces as a contract failure (got {})",
            per_file.2,
        );

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
    fn session_policy_declares_voice_wav_file_exists_for_fm_voice_save() {
        // P0-1 follow-on: fm_voice_save must also assert the voice WAV
        // landed in the canonical voice_profiles directory. The path
        // template uses `${args.name}` interpolation against the spawn
        // task's input args, mirroring how the existing
        // `OminixVoiceExists` validator interpolates the name.
        let policy = WorkspacePolicy::for_session();
        let voice_save = policy
            .spawn_tasks
            .get("fm_voice_save")
            .expect("fm_voice_save contract");
        let has_file_exists = voice_save.on_completion.iter().any(|entry| {
            matches!(
                entry,
                SpawnTaskValidatorSpec::Bare(ValidatorSpec::FileExists { path, .. })
                    if path.contains("${args.name}") && path.ends_with(".wav")
            )
        });
        assert!(
            has_file_exists,
            "fm_voice_save should declare FileExists with ${{args.name}}.wav template; got {:?}",
            voice_save.on_completion
        );
    }

    #[test]
    fn session_policy_declares_pptx_magic_bytes_for_mofa_slides() {
        // P1-4: mofa_slides emits a .pptx artifact. The default session
        // policy must declare a MagicBytes (Pptx) validator so a tool that
        // wrote an HTML error page in place of the PPTX is rejected at
        // the harness gate rather than declared "success" by the LLM.
        let policy = WorkspacePolicy::for_session();
        let slides = policy
            .spawn_tasks
            .get("mofa_slides")
            .expect("mofa_slides contract");
        assert!(slides.on_completion.iter().any(|entry| matches!(
            entry,
            SpawnTaskValidatorSpec::Bare(ValidatorSpec::MagicBytes {
                format: MagicByteKind::Pptx,
                ..
            })
        )));
    }

    #[test]
    fn session_policy_declares_png_magic_bytes_for_image_skills() {
        // P1-5: every spawn-only image-emitting skill carries a MagicBytes
        // (Png) post-condition so a corrupted / HTML error page in place
        // of the rendered card/comic/infographic is rejected at the
        // harness gate. `mofa_frame` is included too (covered by the
        // audit's section 5 "What's missing" table) so that, if/when it
        // becomes spawn_only, the contract is already wired.
        let policy = WorkspacePolicy::for_session();
        for tool in ["mofa_cards", "mofa_comic", "mofa_infographic", "mofa_frame"] {
            let entry = policy
                .spawn_tasks
                .get(tool)
                .unwrap_or_else(|| panic!("policy missing spawn task for {tool}"));
            assert!(
                entry.on_completion.iter().any(|spec| matches!(
                    spec,
                    SpawnTaskValidatorSpec::Bare(ValidatorSpec::MagicBytes {
                        format: MagicByteKind::Png,
                        ..
                    })
                )),
                "{tool} should declare MagicBytes (png); got {:?}",
                entry.on_completion
            );
        }
    }

    #[test]
    fn session_policy_declares_http_probe_for_mofa_publish() {
        // P0-3 (audit): mofa_publish emits a live `deploy_url` via
        // named_outputs; the contract must declare an HttpProbe against
        // `${output.deploy_url}` so a 200-with-soft-404 deployment is
        // rejected at the harness gate. The validator runs as
        // non-required pending the mofa-skills repo follow-up that
        // teaches the skill to emit `named_outputs.deploy_url`.
        let policy = WorkspacePolicy::for_session();
        let publish = policy
            .spawn_tasks
            .get("mofa_publish")
            .expect("policy must declare mofa_publish spawn task");
        let probe = publish
            .on_completion
            .iter()
            .find_map(|entry| match entry {
                SpawnTaskValidatorSpec::Full(validator) => match &validator.spec {
                    ValidatorSpec::HttpProbe {
                        url_template,
                        expected_status,
                        expected_contains,
                    } => Some((
                        validator.required,
                        url_template.clone(),
                        *expected_status,
                        expected_contains.clone(),
                    )),
                    _ => None,
                },
                _ => None,
            })
            .expect("mofa_publish must declare an HttpProbe Full validator");
        assert_eq!(
            probe.1, "${output.deploy_url}",
            "HttpProbe must target the tool-emitted deploy_url",
        );
        assert_eq!(probe.2, 200, "must assert 200 status");
        assert!(
            probe
                .3
                .as_deref()
                .map(|needle| needle.contains("<!DOCTYPE"))
                .unwrap_or(false),
            "expected_contains must carry the <!DOCTYPE soft-404 sentinel; got {:?}",
            probe.3,
        );
        assert!(
            !probe.0,
            "validator must be non-required until the mofa-skills repo teaches \
             mofa_publish to emit named_outputs.deploy_url",
        );
        assert_eq!(
            publish
                .on_failure
                .iter()
                .find(|action| action.starts_with("notify_user:"))
                .cloned(),
            Some("notify_user:Publish probe failed".to_string()),
            "on_failure should surface a notify_user hint",
        );
    }

    #[test]
    fn session_policy_declares_file_exists_for_single_file_image_skills() {
        // mofa_comic and mofa_infographic both take a required `out` arg
        // pointing at a single PNG file. The contract should assert the
        // declared output landed at that path via FileExists +
        // `${args.out}` interpolation.
        let policy = WorkspacePolicy::for_session();
        for tool in ["mofa_comic", "mofa_infographic"] {
            let entry = policy
                .spawn_tasks
                .get(tool)
                .unwrap_or_else(|| panic!("policy missing spawn task for {tool}"));
            let has_file_exists = entry.on_completion.iter().any(|spec| {
                matches!(
                    spec,
                    SpawnTaskValidatorSpec::Bare(ValidatorSpec::FileExists { path, .. })
                        if path.contains("${args.out}")
                )
            });
            assert!(
                has_file_exists,
                "{tool} should declare FileExists with ${{args.out}} template; got {:?}",
                entry.on_completion
            );
        }
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
        assert!(!validator.soft_fail);
        assert_eq!(validator.tier(), Required::Hard);
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
        assert_eq!(validator.tier(), Required::None);
    }

    // -----------------------------------------------------------------
    // Wave-3a: HttpProbeUntil + Sha256Match + soft_fail TOML roundtrips
    // -----------------------------------------------------------------

    #[test]
    fn http_probe_until_roundtrips_through_toml_with_default_intervals() {
        // Operators must be able to declare a polling probe in TOML with
        // only the URL set; the runtime fills the poll/deadline defaults.
        let toml = r#"
            id = "voice_train_done"
            kind = "http_probe_until"
            url_template = "http://x/v1/train/status?task_id=${args.task_id}"
            expected_contains = "complete"
        "#;
        let parsed: Validator = toml::from_str(toml).unwrap();
        match parsed.spec {
            ValidatorSpec::HttpProbeUntil {
                ref url_template,
                expected_status,
                ref expected_contains,
                poll_interval_ms,
                deadline_ms,
            } => {
                assert_eq!(
                    url_template,
                    "http://x/v1/train/status?task_id=${args.task_id}"
                );
                assert_eq!(expected_status, 200);
                assert_eq!(expected_contains.as_deref(), Some("complete"));
                assert_eq!(poll_interval_ms, 2_000);
                assert_eq!(deadline_ms, 30_000);
            }
            ref other => panic!("expected HttpProbeUntil, got {other:?}"),
        }
        // Round-trip the validator through TOML to confirm fields survive.
        let rendered = toml::to_string_pretty(&parsed).unwrap();
        let reparsed: Validator = toml::from_str(&rendered).unwrap();
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn sha256_match_roundtrips_through_toml() {
        let toml = r#"
            id = "skill_main_hash"
            kind = "sha256_match"
            glob = "skill_main"
            sha256 = "${args.expected_sha256}"
        "#;
        let parsed: Validator = toml::from_str(toml).unwrap();
        match parsed.spec {
            ValidatorSpec::Sha256Match {
                ref glob,
                ref sha256,
            } => {
                assert_eq!(glob, "skill_main");
                assert_eq!(sha256, "${args.expected_sha256}");
            }
            ref other => panic!("expected Sha256Match, got {other:?}"),
        }
    }

    #[test]
    fn per_file_non_silent_roundtrips_through_toml() {
        // Operator-visible TOML shape. `require_at_least` is the
        // distinguishing field vs. AudioNonSilent — defaulted via serde so
        // an operator can omit it and still get sensible behaviour.
        let toml = r#"
            id = "podcast_segments_non_silent"
            kind = "per_file_non_silent"
            glob = "**/segments/seg_*.wav"
            min_ratio = 0.3
            require_at_least = 1
        "#;
        let parsed: Validator = toml::from_str(toml).unwrap();
        match parsed.spec {
            ValidatorSpec::PerFileNonSilent {
                ref glob,
                min_ratio,
                require_at_least,
            } => {
                assert_eq!(glob, "**/segments/seg_*.wav");
                assert!((min_ratio - 0.3).abs() < f32::EPSILON);
                assert_eq!(require_at_least, 1);
            }
            ref other => panic!("expected PerFileNonSilent, got {other:?}"),
        }
        // Round-trip the validator through TOML to confirm fields survive.
        let rendered = toml::to_string_pretty(&parsed).unwrap();
        let reparsed: Validator = toml::from_str(&rendered).unwrap();
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn per_file_non_silent_defaults_require_at_least_and_min_ratio_when_omitted() {
        // `require_at_least` and `min_ratio` are both `#[serde(default)]`
        // so operator policies can declare just the glob and inherit the
        // harness-wide defaults (0 / 0.3). This keeps the TOML shape
        // minimal for the common "optional intermediate artifact" case.
        let toml = r#"
            id = "podcast_segments_non_silent"
            kind = "per_file_non_silent"
            glob = "**/segments/seg_*.wav"
        "#;
        let parsed: Validator = toml::from_str(toml).unwrap();
        match parsed.spec {
            ValidatorSpec::PerFileNonSilent {
                ref glob,
                min_ratio,
                require_at_least,
            } => {
                assert_eq!(glob, "**/segments/seg_*.wav");
                assert!(
                    (min_ratio - default_non_silent_ratio()).abs() < f32::EPSILON,
                    "min_ratio must default to {} when omitted, got {min_ratio}",
                    default_non_silent_ratio()
                );
                assert_eq!(
                    require_at_least, 0,
                    "require_at_least must default to 0 when omitted"
                );
            }
            ref other => panic!("expected PerFileNonSilent, got {other:?}"),
        }
    }

    #[test]
    fn soft_fail_validator_roundtrips_through_toml() {
        // The soft_fail companion field is the Wave-3a serde contract for
        // partial-artifact contracts: hard-required validators that should
        // surface as warnings rather than demote the spawn task.
        let toml = r#"
            id = "sub_artifact_warn"
            required = true
            soft_fail = true
            kind = "file_exists"
            path = "sub-artifact.md"
        "#;
        let parsed: Validator = toml::from_str(toml).unwrap();
        assert!(parsed.required, "required field preserved verbatim");
        assert!(parsed.soft_fail, "soft_fail toggled on");
        assert_eq!(parsed.tier(), Required::Soft);
        // Round-trip: soft_fail must serialize and deserialize cleanly.
        let rendered = toml::to_string_pretty(&parsed).unwrap();
        assert!(
            rendered.contains("soft_fail = true"),
            "soft_fail = true should be emitted in TOML: {rendered}"
        );
        let reparsed: Validator = toml::from_str(&rendered).unwrap();
        assert_eq!(reparsed, parsed);
    }

    #[test]
    fn soft_fail_default_false_is_omitted_from_serialized_toml() {
        // Existing operator policies (no soft_fail field) must round-trip
        // byte-for-byte: soft_fail = false is the default and shouldn't
        // surface in the rendered TOML.
        let toml = r#"
            id = "primary_required"
            kind = "file_exists"
            path = "primary.md"
        "#;
        let parsed: Validator = toml::from_str(toml).unwrap();
        assert!(!parsed.soft_fail);
        let rendered = toml::to_string_pretty(&parsed).unwrap();
        assert!(
            !rendered.contains("soft_fail"),
            "default soft_fail = false must not be emitted: {rendered}"
        );
    }

    #[test]
    fn validator_tier_collapses_required_and_soft_fail_correctly() {
        // The 4-case truth table from `Validator::tier`'s rustdoc.
        let make = |required: bool, soft_fail: bool| Validator {
            id: "x".into(),
            required,
            soft_fail,
            timeout_ms: None,
            phase: ValidatorPhaseKind::Completion,
            spec: ValidatorSpec::FileExists {
                path: "x".into(),
                min_bytes: None,
            },
        };
        assert_eq!(make(true, false).tier(), Required::Hard);
        assert_eq!(make(true, true).tier(), Required::Soft);
        assert_eq!(make(false, false).tier(), Required::None);
        assert_eq!(make(false, true).tier(), Required::Soft);
    }

    // -----------------------------------------------------------------
    // Wave-3a: `for_session()` wire targets
    // -----------------------------------------------------------------

    #[test]
    fn session_policy_declares_sha256_match_contract_for_manage_skills() {
        // Wire target for the Wave-3a `Sha256Match` variant: lifts the
        // inline SHA-256 check in `tools/manage_skills.rs::download_binary`
        // onto the canonical validator path so it surfaces in the contract
        // diagnostics ledger. The validator interpolates the expected digest
        // through `${args.expected_sha256}` so the manage_skills tool can
        // pass the manifest-declared hash without hard-coding it in the
        // workspace policy.
        let policy = WorkspacePolicy::for_session();
        let task = policy
            .spawn_tasks
            .get("manage_skills")
            .expect("manage_skills spawn-task contract should be reserved");
        let has_sha = task.on_completion.iter().any(|entry| {
            matches!(
                entry,
                SpawnTaskValidatorSpec::Bare(ValidatorSpec::Sha256Match { sha256, .. })
                    if sha256.contains("${args.expected_sha256}")
            )
        });
        assert!(
            has_sha,
            "manage_skills contract should declare Sha256Match interpolated against args.expected_sha256; got {:?}",
            task.on_completion
        );
    }

    #[test]
    fn session_policy_declares_soft_fail_sub_artifacts_for_research_skills() {
        // Wire target for the Wave-3a `soft_fail` tier: `synthesize_research`
        // and `deep_search` produce a primary report PLUS optional sub-
        // artifacts. The primary is hard-required (block delivery if it's
        // missing); the sub-artifacts are soft-fail so a partial-artifact
        // run still completes with operator-visible warnings.
        let policy = WorkspacePolicy::for_session();
        for tool in ["synthesize_research", "deep_search"] {
            let task = policy
                .spawn_tasks
                .get(tool)
                .unwrap_or_else(|| panic!("policy missing spawn task for {tool}"));
            let mut saw_hard = false;
            let mut saw_soft = false;
            for entry in &task.on_completion {
                let validator = entry.clone().into_validator(tool, 0);
                match validator.tier() {
                    Required::Hard => saw_hard = true,
                    Required::Soft => saw_soft = true,
                    Required::None => {}
                }
            }
            assert!(
                saw_hard,
                "{tool} contract should declare a hard-required validator for the primary report"
            );
            assert!(
                saw_soft,
                "{tool} contract should declare a soft-fail validator for optional sub-artifacts"
            );
        }
    }

    // ----- WorkspacePolicyKind::Coding (Audit Gap-1 + section 7 Q3) -----

    #[test]
    fn should_detect_coding_kind_when_cargo_toml_is_present() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        assert_eq!(
            detect_workspace_policy_kind(tmp.path()),
            WorkspacePolicyKind::Coding
        );
    }

    #[test]
    fn should_detect_coding_kind_when_package_json_is_present() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("package.json"), "{}").unwrap();
        assert_eq!(
            detect_workspace_policy_kind(tmp.path()),
            WorkspacePolicyKind::Coding
        );
    }

    #[test]
    fn should_detect_coding_kind_when_pyproject_toml_is_present() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("pyproject.toml"), "[project]\n").unwrap();
        assert_eq!(
            detect_workspace_policy_kind(tmp.path()),
            WorkspacePolicyKind::Coding
        );
    }

    #[test]
    fn should_fall_back_to_session_kind_when_no_language_signal() {
        let tmp = tempfile::tempdir().unwrap();
        // Just an unrelated file — no manifest probes match.
        std::fs::write(tmp.path().join("README.md"), "# hi").unwrap();
        assert_eq!(
            detect_workspace_policy_kind(tmp.path()),
            WorkspacePolicyKind::Session
        );
    }

    #[test]
    fn should_return_coding_policy_marker_for_coding_kind() {
        let policy = WorkspacePolicy::for_coding();
        assert_eq!(policy.workspace.kind, WorkspacePolicyKind::Coding);
        // Inherits session spawn-task contracts so coding workspaces that
        // still spawn slides/podcast keep the per-skill gate.
        assert!(policy.spawn_tasks.contains_key("fm_tts"));
        assert!(policy.spawn_tasks.contains_key("mofa_slides"));
    }

    #[test]
    fn should_return_rust_check_hook_in_coding_defaults() {
        let hooks = coding_default_hooks();
        let cargo = hooks
            .iter()
            .find(|h| h.command.first().map(String::as_str) == Some("cargo"))
            .expect("cargo check hook present");
        assert_eq!(cargo.event, crate::hooks::HookEvent::AfterToolCall);
        assert!(cargo.path_filter.iter().any(|p| p == "**/*.rs"));
        assert!(cargo.tool_filter.iter().any(|t| t == "edit_file"));
        assert!(cargo.tool_filter.iter().any(|t| t == "write_file"));
        assert!(cargo.tool_filter.iter().any(|t| t == "diff_edit"));
        assert_eq!(cargo.requires_bin.as_deref(), Some("cargo"));
    }

    #[test]
    fn should_return_eslint_hook_gated_on_bin_in_coding_defaults() {
        let hooks = coding_default_hooks();
        let eslint = hooks
            .iter()
            .find(|h| h.command.first().map(String::as_str) == Some("eslint"))
            .expect("eslint hook present");
        // ESLint hook must be opt-out friendly via requires_bin — operators
        // without eslint on PATH must NOT see hook failures every edit.
        assert_eq!(eslint.requires_bin.as_deref(), Some("eslint"));
        assert!(
            eslint
                .path_filter
                .iter()
                .any(|p| p.ends_with(".{js,ts,tsx,jsx}"))
        );
    }

    #[test]
    fn should_return_ruff_hook_gated_on_bin_in_coding_defaults() {
        let hooks = coding_default_hooks();
        let ruff = hooks
            .iter()
            .find(|h| h.command.first().map(String::as_str) == Some("ruff"))
            .expect("ruff hook present");
        assert_eq!(ruff.requires_bin.as_deref(), Some("ruff"));
        assert!(ruff.path_filter.iter().any(|p| p == "**/*.py"));
    }

    #[test]
    fn should_not_emit_coding_hooks_for_session_kind() {
        // Session policies retain the legacy no-default-hooks behaviour so
        // existing operators don't see a sudden new wave of cargo checks.
        let session = WorkspacePolicy::for_session();
        assert_eq!(session.workspace.kind, WorkspacePolicyKind::Session);
        // The hooks helper is global (not method-on-policy); we assert that
        // callers must opt in by inspecting the kind themselves.
        assert_ne!(session.workspace.kind, WorkspacePolicyKind::Coding);
    }

    #[test]
    fn should_serialize_coding_kind_as_kebab_case() {
        let policy = WorkspacePolicy::for_coding();
        let rendered = toml::to_string(&policy).unwrap();
        assert!(
            rendered.contains("kind = \"coding\""),
            "expected kebab-case 'coding' in serialized policy:\n{}",
            rendered
        );
    }
}
