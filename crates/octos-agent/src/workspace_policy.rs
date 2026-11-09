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

/// Where a file-list-driven validator (`MagicBytes`, `AudioNonSilent`,
/// `PerFileNonSilent`) sources its candidate file paths.
///
/// Defaults to [`ValidatorFileSource::Glob`] so existing TOML contracts keep
/// their historic behaviour: the validator resolves the `glob` field against
/// the workspace root. Opting into [`ValidatorFileSource::SpawnOnlyFiles`]
/// tells the validator to use the originating spawn_only tool's
/// `files_to_send` list verbatim (the plugin protocol's authoritative list
/// of files the skill just produced), optionally filtered by an extension
/// suffix.
///
/// Issue octos #1034: the topic-suffixed plugin output paths
/// (e.g. `mofa-podcast-zhuyu/`) break globbing because the per-topic
/// directory name is unpredictable. `files_to_send` carries the exact path
/// the plugin wrote and is the canonical source of truth.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidatorFileSource {
    /// Resolve files by glob (the legacy behaviour). The validator's `glob`
    /// field is matched against the workspace root.
    #[default]
    Glob,
    /// Use the originating spawn_only tool's `files_to_send` list, optionally
    /// filtered by a file extension supplied alongside (e.g. `extension =
    /// "mp3"`). The `glob` field is IGNORED in this mode. When the file list
    /// is empty (non-spawn-only contexts) the validator surfaces a `Fail`
    /// outcome so misconfigured policies are caught early.
    SpawnOnlyFiles,
}

impl ValidatorFileSource {
    /// Predicate used by `skip_serializing_if` so a default value does not
    /// pollute TOML output for contracts that did not opt in.
    pub fn is_default(&self) -> bool {
        matches!(self, Self::Glob)
    }
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
    ///
    /// When `source = "spawn_only_files"` (octos #1034) the validator skips
    /// the glob entirely and consumes the originating spawn_only tool's
    /// `files_to_send` list, optionally narrowed by `extension`
    /// (e.g. `extension = "mp3"`). This is the canonical mode for plugins
    /// that write to topic-suffixed output directories whose names a glob
    /// cannot predict.
    AudioNonSilent {
        #[serde(default)]
        glob: String,
        #[serde(default = "default_non_silent_ratio")]
        min_ratio: f32,
        #[serde(default, skip_serializing_if = "ValidatorFileSource::is_default")]
        source: ValidatorFileSource,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        extension: Option<String>,
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
        #[serde(default)]
        glob: String,
        #[serde(default = "default_non_silent_ratio")]
        min_ratio: f32,
        #[serde(default)]
        require_at_least: usize,
        /// Where to source the candidate file list. See [`ValidatorFileSource`]
        /// for the opt-in `spawn_only_files` mode (octos #1034) that bypasses
        /// the glob and consumes the plugin's `files_to_send` list verbatim.
        #[serde(default, skip_serializing_if = "ValidatorFileSource::is_default")]
        source: ValidatorFileSource,
        /// Optional extension filter applied to the file list when
        /// `source = "spawn_only_files"`. Ignored when `source = "glob"`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        extension: Option<String>,
    },
    /// Assert each file matching `glob` has the magic-byte prefix for the
    /// declared `format`. Catches "tool wrote 0 bytes" or "tool wrote an
    /// HTML error page in place of an MP3".
    ///
    /// The format field is named `format` rather than `kind` to avoid
    /// colliding with serde's `kind` discriminator tag.
    ///
    /// When `source = "spawn_only_files"` (octos #1034) the validator skips
    /// the glob entirely and consumes the originating spawn_only tool's
    /// `files_to_send` list, optionally narrowed by `extension`.
    MagicBytes {
        #[serde(default)]
        glob: String,
        format: MagicByteKind,
        #[serde(default, skip_serializing_if = "ValidatorFileSource::is_default")]
        source: ValidatorFileSource,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        extension: Option<String>,
    },
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
                    // Path-based `file_exists` checks were removed: the
                    // deck lands under `<workspace>/skill-output/slides/<slug>/`
                    // via the host's plugin work-dir rebind (outside the
                    // project dir), so the previous `file_exists:output/...`
                    // could never match in production. The MagicBytes
                    // validator below now consumes the plugin's
                    // `files_to_send` list via the SpawnOnlyFiles source
                    // — the same path set the session-scope
                    // `mofa_slides_contract` uses.
                    on_completion: Vec::new(),
                    // octos #997: gate the slides project on the PPTX
                    // magic-bytes signature so an HTML "success" deck
                    // trips the contract. Uses `SpawnOnlyFiles` source
                    // — the spawn loop wires `files_to_send` through to
                    // `run_project_root_validators`, which filters to
                    // files belonging to this project
                    // (`<session>/skill-output/slides/<slug>/` or the
                    // legacy `<session>/slides/<slug>/`) before passing
                    // them to the validator runner.
                    validators: vec![Validator {
                        id: "slides.mofa_slides.pptx_magic_bytes".into(),
                        required: true,
                        soft_fail: false,
                        timeout_ms: None,
                        phase: ValidatorPhaseKind::Completion,
                        spec: ValidatorSpec::MagicBytes {
                            glob: String::new(),
                            format: MagicByteKind::Pptx,
                            source: ValidatorFileSource::SpawnOnlyFiles,
                            extension: Some("pptx".into()),
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
        // octos #1040 (follow-up to #1035 / #1037): `mofa_comic`,
        // `mofa_infographic`, and `mofa_frame` all emit a single PNG via
        // `files_to_send`. The `image_png` artifact gives those contracts
        // a target name so `bind_explicit_files_to_artifacts` can bind the
        // reported PNG path into `ActionContext` without erroring on "no
        // artifact source" the same way `slides_pptx` does for slides.
        artifacts.insert("image_png".into(), "**/*.png".into());

        // fm_tts emits a single MP3 deliverable via the plugin protocol's
        // files_to_send list. Validate that exact reported file instead of a
        // broad workspace glob, which can match stale audio from an earlier
        // call in the same session workspace.
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
            on_completion: vec![SpawnTaskValidatorSpec::Bare(
                ValidatorSpec::AudioNonSilent {
                    glob: String::new(),
                    min_ratio: default_non_silent_ratio(),
                    source: ValidatorFileSource::SpawnOnlyFiles,
                    extension: Some("mp3".into()),
                },
            )],
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
            //
            // octos #1034: switched (1) and (2) to consume the plugin's
            // `files_to_send` list (the canonical authoritative path set
            // emitted by the JSON envelope on stdout). The prior hardcoded
            // glob `skill-output/mofa-podcast/**/*.mp3` could not match
            // topic-suffixed output directories (e.g. `mofa-podcast-zhuyu/`
            // for the chat topic 《逐玉》), and PR #1014's `*→**` widening
            // did not cure the prefix mismatch. `files_to_send` carries
            // the exact path the plugin wrote, so the validator no longer
            // races the plugin's directory-naming logic.
            //
            // The historical (3) was a `PerFileNonSilent` gate over
            // `**/segments/seg_*.wav` to catch the silent-segment dropout
            // class that whole-file AudioNonSilent can mask. It was dropped
            // here: the deployed `mofa-podcast` plugin does not preserve
            // `seg_*.wav` segment files on disk after assembly, so the
            // gate matched zero files and the `require_at_least:1` floor
            // hard-failed every podcast call (regression observed live on
            // mini3 2026-05-18: `per_file_non_silent: expected >=1 audio
            // files, found 0`). The plugin's silent-segment failure mode
            // can be re-armed in a future contract once the skill emits
            // segment scratch files (or carries them in `files_to_send`).
            on_completion: vec![
                SpawnTaskValidatorSpec::Bare(ValidatorSpec::MagicBytes {
                    glob: String::new(),
                    format: MagicByteKind::Mp3,
                    source: ValidatorFileSource::SpawnOnlyFiles,
                    extension: Some("mp3".into()),
                }),
                SpawnTaskValidatorSpec::Bare(ValidatorSpec::AudioNonSilent {
                    glob: String::new(),
                    min_ratio: default_non_silent_ratio(),
                    source: ValidatorFileSource::SpawnOnlyFiles,
                    extension: Some("mp3".into()),
                }),
            ],
        };

        // Voice synthesis (LLM-driven TTS): assert the decoded audio is not
        // silent. Catches the "render produced empty audio" failure path.
        //
        // octos #1038 (follow-up to #1037): consume the plugin's
        // `files_to_send` envelope so the AudioNonSilent check runs against
        // the exact audio path the skill reported, not whatever happens to
        // match a recursive `skill-output/voice/**/*.{mp3,wav}` glob. The
        // voice skill writes to `skill-output/voice/<timestamp>.{wav,mp3}`,
        // so a recursive glob could match a stale file from an earlier run
        // in the same session workspace and falsely satisfy the contract
        // for a freshly-failed call — the same structural fragility we
        // already closed for `podcast_generate` (#1034) and `mofa_slides`
        // (#1036).
        //
        // `extension = None` — the voice plugin emits a single audio path
        // per call (`try_convert_to_mp3` deletes the .wav when ffmpeg
        // succeeds, so the .mp3 and .wav cannot co-exist by construction).
        // We need to accept either extension since the macOS Say fallback
        // path also routes through `try_convert_to_mp3` and ffmpeg may not
        // be installed, in which case `final_path` stays a .wav.
        //
        // PR #1037 originally swept this contract alongside `mofa_slides`
        // but the voice half was reverted at 772783e7 — the plugin
        // emitted a `Generated audio: <path>` success line that
        // `PluginTool::detect_output_file` did not recognise. PR #1039
        // (this PR's predecessor on the stack) fixed the plugin to emit
        // `Generated: <path>` on its own line, so the detector now
        // populates `files_to_send` and this flip is finally safe.
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
                    glob: String::new(),
                    min_ratio: default_non_silent_ratio(),
                    source: ValidatorFileSource::SpawnOnlyFiles,
                    extension: None,
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
        //
        // octos #1036 (follow-up to #1034 / PR #1035): consume the plugin's
        // `files_to_send` envelope so the MagicBytes check runs against the
        // exact PPTX path the skill reported, not whatever happens to match
        // a recursive `**/*.pptx` glob. Two failure modes the prior glob
        // could mask:
        //   1. A stale `.pptx` from an earlier run in the same session
        //      workspace passing the contract for a freshly-failed call.
        //   2. The plugin writing the deck to a non-default subdirectory
        //      that an operator-customised workspace policy would otherwise
        //      need to anticipate in its glob.
        // The `extension = "pptx"` filter narrows the file list to slide
        // decks if the skill also surfaces auxiliary files (preview PNGs,
        // etc.) via `files_to_send`.
        let mofa_slides_contract = WorkspaceSpawnTaskPolicy {
            artifact: Some("slides_pptx".into()),
            artifacts: Vec::new(),
            on_verify: Vec::new(),
            on_complete: vec![],
            on_deliver: vec![],
            on_failure: vec!["notify_user:Slide generation failed".into()],
            on_completion: vec![SpawnTaskValidatorSpec::Bare(ValidatorSpec::MagicBytes {
                glob: String::new(),
                format: MagicByteKind::Pptx,
                source: ValidatorFileSource::SpawnOnlyFiles,
                extension: Some("pptx".into()),
            })],
        };

        // mofa_cards writes one PNG per card into a `card_dir` and now
        // reports every generated PNG via `files_to_send` (octos #1041).
        // Consume that explicit list so stale PNGs elsewhere in the
        // workspace cannot satisfy a failed card run.
        let mofa_cards_contract = WorkspaceSpawnTaskPolicy {
            artifact: Some("image_png".into()),
            artifacts: Vec::new(),
            on_verify: Vec::new(),
            on_complete: vec![],
            on_deliver: vec![],
            on_failure: vec!["notify_user:Card generation failed".into()],
            on_completion: vec![SpawnTaskValidatorSpec::Bare(ValidatorSpec::MagicBytes {
                glob: String::new(),
                format: MagicByteKind::Png,
                source: ValidatorFileSource::SpawnOnlyFiles,
                extension: Some("png".into()),
            })],
        };

        // mofa_comic and mofa_infographic each take a required `out` arg
        // pointing at a single PNG file. The contract asserts BOTH that
        // the file landed at the declared path (FileExists with
        // `${args.out}`, which already supports template interpolation)
        // AND that the file at the plugin-reported path carries a valid
        // PNG header (MagicBytes against the `spawn_only_files` source —
        // octos #1040, follow-up to #1035 / #1037).
        //
        // `detect_output_file` in `plugins/tool.rs:556-577` populates
        // `files_to_send` from the `args.out` argument when the plugin's
        // success envelope omits it. mofa-cli's `plugin_comic` /
        // `plugin_infographic` emit `"Generated comic: <path>"` /
        // `"Generated infographic: <path>"` (no `Generated:` marker),
        // but the `args.out` branch fires first so the validator always
        // sees the exact PNG path the skill wrote. The `extension =
        // "png"` filter narrows the list if a future revision of the
        // plugin starts surfacing auxiliary files (intermediate panel
        // PNGs in `work_dir`, layout previews, etc.) via
        // `files_to_send`.
        //
        // FileExists still does the per-task path check (mismatches
        // between `args.out` and what the plugin actually wrote);
        // MagicBytes still does the bytes-are-actually-a-PNG check.
        let mofa_comic_contract = WorkspaceSpawnTaskPolicy {
            artifact: Some("image_png".into()),
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
                    glob: String::new(),
                    format: MagicByteKind::Png,
                    source: ValidatorFileSource::SpawnOnlyFiles,
                    extension: Some("png".into()),
                }),
            ],
        };

        let mofa_infographic_contract = WorkspaceSpawnTaskPolicy {
            artifact: Some("image_png".into()),
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
                    glob: String::new(),
                    format: MagicByteKind::Png,
                    source: ValidatorFileSource::SpawnOnlyFiles,
                    extension: Some("png".into()),
                }),
            ],
        };

        // mofa_frame is NOT spawn_only today (so this contract is dormant
        // for the current manifest), but the audit's section-5 missing
        // table lists it as an artifact-producing tool whose contract
        // should be wired even if the gate is not yet fired. Recording
        // the entry here lets the next spawn_only flip pick up the
        // contract automatically.
        //
        // octos #1040: preemptively use the `spawn_only_files` source so
        // the contract is on the right shape the moment the manifest
        // flips. The mofa-frame plugin script
        // (`mofa-skills/mofa-frame/main`) already emits `files_to_send`
        // in its JSON envelope (the final `jq` call surfaces the
        // generated PNG path), so no plugin work is needed.
        let mofa_frame_contract = WorkspaceSpawnTaskPolicy {
            artifact: Some("image_png".into()),
            artifacts: Vec::new(),
            on_verify: Vec::new(),
            on_complete: vec![],
            on_deliver: vec![],
            on_failure: vec!["notify_user:Frame extraction failed".into()],
            on_completion: vec![SpawnTaskValidatorSpec::Bare(ValidatorSpec::MagicBytes {
                glob: String::new(),
                format: MagicByteKind::Png,
                source: ValidatorFileSource::SpawnOnlyFiles,
                extension: Some("png".into()),
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
            // Four explicit patterns: the `glob` crate treats braces as
            // LITERALS, so "**/*.{js,ts,tsx,jsx}" matches no real file
            // (#2129 review, finding 10 — verified empirically).
            path_filter: vec![
                "**/*.js".into(),
                "**/*.ts".into(),
                "**/*.tsx".into(),
                "**/*.jsx".into(),
            ],
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
    let mut policy: WorkspacePolicy = toml::from_str(&raw)
        .wrap_err_with(|| format!("parse workspace policy failed: {}", path.display()))?;
    check_supported(
        "WorkspacePolicy",
        policy.schema_version,
        WORKSPACE_POLICY_SCHEMA_VERSION,
    )
    .wrap_err_with(|| format!("incompatible workspace policy: {}", path.display()))?;

    // Auto-migrate pre-#997-round-3 slides policies on read. Existing
    // deployments (e.g. mini3 dspfac) carry `.octos-workspace.toml` files
    // with `file_exists:output/deck.pptx` on_completion entries and/or
    // Glob-source MagicBytes validators — neither matches the production
    // location (deck lands under `<workspace>/skill-output/...` via the
    // host's plugin work-dir rebind). Detect those markers, rebuild the
    // policy with slug-aware skill-output artifact paths + a
    // SpawnOnlyFiles MagicBytes validator, and persist back to disk so
    // the next read is cheap and the on-disk policy reflects the new
    // contract.
    if policy.workspace.kind == WorkspacePolicyKind::Slides && is_legacy_slides_policy(&policy) {
        if let Some(slug) = project_root.file_name().and_then(|n| n.to_str()) {
            let upgraded = build_modern_slides_policy(slug);
            // Best-effort persist — a write failure (read-only mount,
            // race with another writer) should not block the policy
            // load; the in-memory upgrade still applies for this caller.
            if let Err(error) = write_workspace_policy_force(project_root, &upgraded) {
                tracing::warn!(
                    project_root = %project_root.display(),
                    error = %error,
                    "auto-migration of slides policy: failed to persist upgrade"
                );
            } else {
                tracing::info!(
                    project_root = %project_root.display(),
                    "migrated legacy slides workspace policy to skill-output / SpawnOnlyFiles contract"
                );
            }
            policy = upgraded;
        }
    }

    Ok(Some(policy))
}

/// Detect a pre-#997-round-3 slides policy: declares
/// `file_exists:output/...` `on_completion` checks (the broken project-
/// rooted path that the host's plugin rebind never satisfies) OR carries
/// a Glob-source MagicBytes validator (which can't see decks under
/// `skill-output/`).
fn is_legacy_slides_policy(policy: &WorkspacePolicy) -> bool {
    let has_legacy_on_completion = policy
        .validation
        .on_completion
        .iter()
        .any(|s| s.starts_with("file_exists:output/"));
    let has_glob_pptx_validator = policy.validation.validators.iter().any(|v| {
        matches!(
            &v.spec,
            ValidatorSpec::MagicBytes {
                format: MagicByteKind::Pptx,
                source: ValidatorFileSource::Glob,
                ..
            }
        )
    });
    has_legacy_on_completion || has_glob_pptx_validator
}

/// Build the post-migration slides policy with slug-aware skill-output
/// artifact paths and the SpawnOnlyFiles MagicBytes validator. Mirrors
/// what `slides_delivery::workspace_policy_for_slug(Some(slug))` returns
/// from the CLI crate, kept inline here to avoid a layer flip
/// (octos-agent must not depend on octos-cli).
fn build_modern_slides_policy(slug: &str) -> WorkspacePolicy {
    let mut policy = WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides);
    policy.artifacts = WorkspaceArtifactsPolicy {
        entries: BTreeMap::from([
            (
                "primary".into(),
                format!("skill-output/slides/{slug}/output/deck.pptx"),
            ),
            (
                "deck".into(),
                format!("skill-output/slides/{slug}/output/deck.pptx"),
            ),
            (
                "previews".into(),
                format!("skill-output/slides/{slug}/output/**/slide-*.png"),
            ),
        ]),
    };
    policy
}

/// `write_workspace_policy` without the `create_new` guard — used by the
/// auto-migration path to OVERWRITE an existing legacy file.
fn write_workspace_policy_force(project_root: &Path, policy: &WorkspacePolicy) -> Result<()> {
    let path = workspace_policy_path(project_root);
    let rendered = toml::to_string_pretty(policy)
        .wrap_err_with(|| format!("serialize workspace policy failed: {}", path.display()))?;
    let tmp_path = path.with_extension("toml.tmp");
    std::fs::write(&tmp_path, rendered.as_bytes())
        .wrap_err_with(|| format!("write workspace policy tmp failed: {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, &path)
        .wrap_err_with(|| format!("rename workspace policy failed: {}", path.display()))?;
    Ok(())
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
