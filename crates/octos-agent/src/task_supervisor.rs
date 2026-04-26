//! Background task lifecycle management for spawn_only tools.
//!
//! The `TaskSupervisor` is a status store that tracks background tasks from
//! spawn to completion. It does NOT enforce workspace contracts — that
//! responsibility belongs to `workspace_contract::enforce()`, which runs
//! inline in `execution.rs` BEFORE the supervisor status is updated.
//!
//! The supervisor only sees truth-checked states: `Completed` means the
//! workspace contract was satisfied, `Failed` means it was not.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use metrics::counter;
use octos_core::TaskId;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::harness_events::{HarnessEvent, HarnessEventPayload};
use crate::progress::{ProgressEvent, ProgressReporter};

const CURRENT_TASK_LEDGER_SCHEMA: u32 = 1;

/// Lifecycle status of a background task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Spawned,
    Running,
    Completed,
    Failed,
}

impl TaskStatus {
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Spawned | Self::Running)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Spawned => "spawned",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

/// Structured terminal outcome for a child session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChildSessionTerminalState {
    Completed,
    RetryableFailure,
    TerminalFailure,
}

/// Join state for a child session contract.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChildSessionJoinState {
    Joined,
    Orphaned,
}

/// Explicit follow-up policy for terminal child-session failures.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChildSessionFailureAction {
    Retry,
    Escalate,
}

/// Fine-grained runtime phase of a background task.
///
/// `status` remains the coarse externally stable summary, while
/// `runtime_state` tracks where the task is inside the workspace/runtime
/// lifecycle. This lets the agent and UI distinguish "tool is still running"
/// from "tool finished but outputs are still being verified/delivered".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskRuntimeState {
    Spawned,
    ExecutingTool,
    ResolvingOutputs,
    VerifyingOutputs,
    DeliveringOutputs,
    CleaningUp,
    Completed,
    Failed,
}

/// Stable externally-facing lifecycle state for background tasks.
///
/// This is the coarse public contract that callers and UIs should consume.
/// It intentionally groups several internal runtime phases under `verifying`
/// so the runtime can evolve without leaking extra state-machine detail.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskLifecycleState {
    Queued,
    Running,
    Verifying,
    Ready,
    Failed,
}

/// A tracked background task spawned by a spawn_only tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundTask {
    pub id: String,
    pub tool_name: String,
    pub tool_call_id: String,
    /// Parent session that owns this task.
    pub parent_session_key: Option<String>,
    /// Stable child session key derived from the parent session and task id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_session_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_terminal_state: Option<ChildSessionTerminalState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_join_state: Option<ChildSessionJoinState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_joined_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_failure_action: Option<ChildSessionFailureAction>,
    /// Append-only ledger path used to persist this task's snapshots.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_ledger_path: Option<String>,
    pub status: TaskStatus,
    pub runtime_state: TaskRuntimeState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_detail: Option<String>,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub output_files: Vec<String>,
    pub error: Option<String>,
    /// Session that owns this task (for per-session filtering).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_key: Option<String>,
    /// Original tool arguments — preserved so failure-recovery flows can
    /// surface the exact input the LLM passed when offering alternatives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_input: Option<Value>,
}

impl BackgroundTask {
    pub fn lifecycle_state(&self) -> TaskLifecycleState {
        match self.status {
            TaskStatus::Spawned => TaskLifecycleState::Queued,
            TaskStatus::Completed => TaskLifecycleState::Ready,
            TaskStatus::Failed => TaskLifecycleState::Failed,
            TaskStatus::Running => match self.runtime_state {
                TaskRuntimeState::Spawned | TaskRuntimeState::ExecutingTool => {
                    TaskLifecycleState::Running
                }
                TaskRuntimeState::ResolvingOutputs
                | TaskRuntimeState::VerifyingOutputs
                | TaskRuntimeState::DeliveringOutputs
                | TaskRuntimeState::CleaningUp
                | TaskRuntimeState::Completed => TaskLifecycleState::Verifying,
                TaskRuntimeState::Failed => TaskLifecycleState::Failed,
            },
        }
    }
}

/// Callback invoked when a task's status changes.
type OnChangeCallback = Box<dyn Fn(&BackgroundTask) + Send + Sync>;

/// Payload emitted when a `spawn_only` background task transitions to
/// `Failed`. Consumers (e.g. the session actor) use this to schedule a
/// synthetic recovery turn so the LLM can re-engage with an actionable
/// error and offer alternatives instead of leaving the user stuck on a
/// terminal-only failure notification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpawnOnlyFailureSignal {
    /// Background task identifier (matches `BackgroundTask::id`).
    pub task_id: String,
    /// Tool that failed (e.g. `fm_tts`).
    pub tool_name: String,
    /// The original tool arguments passed by the LLM when invoking the tool.
    /// May be `Value::Null` if the input was not captured for this task.
    pub tool_input: Value,
    /// The textual error reported by the tool, contract validator, or wrapper.
    pub error_message: String,
    /// Best-effort list of alternatives extracted from the error text via the
    /// `available: X, Y, Z` pattern. Empty when no alternatives were detected.
    pub suggested_alternatives: Vec<String>,
    /// Owning session, when the failed task is bound to one.
    pub parent_session_key: Option<String>,
}

/// Callback invoked when a `spawn_only` task fails. Receives the structured
/// signal payload so consumers can build a recovery prompt without re-parsing
/// the raw `BackgroundTask`.
type OnFailureCallback = Box<dyn Fn(&SpawnOnlyFailureSignal) + Send + Sync>;

/// Extract a list of alternatives from a tool error message using the simple
/// `available: X, Y, Z` pattern. Returns an empty vector when no match is
/// found so callers can fall back to surfacing the raw error text.
///
/// This is intentionally conservative — we only handle the canonical
/// "available: ..." phrasing emitted by the fm_tts/voice-skill family. More
/// aggressive parsing belongs in the failure-modes inventory follow-up.
pub fn parse_alternatives(error_text: &str) -> Vec<String> {
    // Use a literal scan rather than a regex so we don't pull in a fresh
    // dependency or risk pathological backtracking. The marker is
    // case-insensitive and matched anywhere in the message.
    let needle = "available:";
    let lower = error_text.to_lowercase();
    let Some(start) = lower.find(needle) else {
        return Vec::new();
    };
    let tail = &error_text[start + needle.len()..];

    // Stop at the first sentence boundary so we don't grab the entire
    // remainder of the error message. Newlines and periods both terminate
    // the alternatives clause.
    let stop = tail.find(['\n', '.', ';']).unwrap_or(tail.len());
    let clause = &tail[..stop];

    clause
        .split(',')
        .map(|item| item.trim().trim_matches(['"', '\'']))
        .filter(|item| !item.is_empty())
        .map(|item| item.to_string())
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedTaskRecord {
    #[serde(default = "default_task_ledger_schema")]
    schema_version: u32,
    task: BackgroundTask,
}

fn default_task_ledger_schema() -> u32 {
    CURRENT_TASK_LEDGER_SCHEMA
}

fn record_child_session_lifecycle(kind: &'static str, outcome: &'static str) {
    counter!(
        "octos_child_session_lifecycle_total",
        "kind" => kind.to_string(),
        "outcome" => outcome.to_string()
    )
    .increment(1);
}

fn record_child_session_orphan(reason: &'static str) {
    counter!(
        "octos_child_session_orphan_total",
        "reason" => reason.to_string()
    )
    .increment(1);
}

fn record_workflow_phase_transition(workflow_kind: &str, from_phase: &str, to_phase: &str) {
    counter!(
        "octos_workflow_phase_transition_total",
        "workflow_kind" => workflow_kind.to_string(),
        "from_phase" => from_phase.to_string(),
        "to_phase" => to_phase.to_string()
    )
    .increment(1);
}

fn workflow_labels(detail: Option<&str>) -> (Option<String>, Option<String>) {
    let parsed = detail
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .unwrap_or(Value::Null);
    let workflow_kind = parsed
        .get("workflow_kind")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let current_phase = parsed
        .get("current_phase")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    (workflow_kind, current_phase)
}

fn child_terminal_kind_label(state: &ChildSessionTerminalState) -> &'static str {
    match state {
        ChildSessionTerminalState::Completed => "completed",
        ChildSessionTerminalState::RetryableFailure => "retryable_failed",
        ChildSessionTerminalState::TerminalFailure => "terminal_failed",
    }
}

fn child_join_outcome_label(state: &ChildSessionJoinState) -> &'static str {
    match state {
        ChildSessionJoinState::Joined => "joined",
        ChildSessionJoinState::Orphaned => "orphaned",
    }
}

fn child_failure_action_for_terminal_state(
    state: &ChildSessionTerminalState,
) -> Option<ChildSessionFailureAction> {
    match state {
        ChildSessionTerminalState::Completed => None,
        ChildSessionTerminalState::RetryableFailure => Some(ChildSessionFailureAction::Retry),
        ChildSessionTerminalState::TerminalFailure => Some(ChildSessionFailureAction::Escalate),
    }
}

/// Coarse MIME class used to gate background-task artifact size validation.
///
/// Spawn_only skills occasionally report `success: true` with malformed or
/// empty artifacts (e.g. a 44-byte WAV stub, zero-byte PNG). The supervisor
/// uses this classification as a belt-and-suspenders truth check before it
/// transitions a task to `Completed`, independent of the per-workspace
/// contract that can be missing or misconfigured at deploy time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactMimeClass {
    Audio,
    Image,
    Video,
    Other,
}

impl ArtifactMimeClass {
    /// Minimum byte count required for an artifact of this class. Values
    /// line up with the smallest sane outputs observed in production
    /// skills (e.g. a MIDI-length audio clip, a 1x1 PNG metadata block).
    pub fn min_bytes(self) -> u64 {
        match self {
            Self::Audio => 1024,
            Self::Image => 512,
            Self::Video => 8192,
            Self::Other => 1,
        }
    }

    /// Classify a path by its extension. Unknown or missing extensions fall
    /// back to `Other` so skills that produce bespoke formats still pass a
    /// non-empty check.
    pub fn from_path(path: &Path) -> Self {
        let Some(extension) = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
        else {
            return Self::Other;
        };
        match extension.as_str() {
            "wav" | "mp3" | "m4a" | "ogg" | "opus" | "flac" | "aac" => Self::Audio,
            "png" | "jpg" | "jpeg" | "webp" => Self::Image,
            "mp4" | "mov" | "webm" => Self::Video,
            _ => Self::Other,
        }
    }
}

/// Minimum acceptable WAV duration in seconds. Anything shorter is almost
/// certainly a failed-generation stub — ominix-api's silent-voice bug, for
/// example, occasionally emits a valid 0.05s WAV when the voice is missing.
const MIN_AUDIO_DURATION_SECS: f64 = 0.2;

/// When we sample the PCM payload, reject the clip if fewer than this
/// fraction of samples exceed `SILENCE_SAMPLE_THRESHOLD`. 10% of the first
/// 4 KB ensures even quiet but real voice passes, while pure-silence WAVs
/// are caught before we hand them to the user.
const MIN_NON_SILENT_SAMPLE_RATIO: f64 = 0.10;

/// Absolute value above which a 16-bit sample counts as non-silent. 256 is
/// well below normal speech amplitude (~5000-30000) but comfortably above
/// idle codec noise (~0-20).
const SILENCE_SAMPLE_THRESHOLD: i16 = 256;

/// Size of the PCM slice sampled for the silence check.
const SILENCE_SAMPLE_BYTES: usize = 4096;

/// Validate reported artifacts against the MIME-class size contract.
///
/// Returns `Ok(())` when every artifact exists and satisfies its class's
/// minimum size plus any format-specific content checks. The first failing
/// artifact produces a structured error string with stable shapes:
///
/// - `"Skill reported success but artifact '{path}' failed validation: missing"`
/// - `"Skill reported success but artifact '{path}' failed validation: size_{N}_below_{M}"`
/// - `"Skill reported success but artifact '{path}' failed validation: not_a_valid_wav_container"`
/// - `"Skill reported success but artifact '{path}' failed validation: mp3_magic_missing"`
/// - `"Skill reported success but artifact '{path}' failed validation: m4a_ftyp_missing"`
/// - `"Skill reported success but artifact '{path}' failed validation: ogg_magic_missing"`
/// - `"Skill reported success but artifact '{path}' failed validation: flac_magic_missing"`
/// - `"Skill reported success but artifact '{path}' failed validation: audio_appears_to_be_silence"`
/// - `"Skill reported success but artifact '{path}' failed validation: duration_{N}ms_below_{M}ms"`
///
/// An empty slice passes through (no artifacts to check) — callers handle
/// the "no artifacts" case separately via the contract layer.
///
/// Validation is layered into three cheap tiers:
///
/// 1. Size floor from [`ArtifactMimeClass::min_bytes`].
/// 2. Format magic-number matches extension (WAV/MP3/M4A/OGG/FLAC).
/// 3. For WAV only: duration >= `MIN_AUDIO_DURATION_SECS` and non-silent PCM.
///
/// Tier 3 is skipped for compressed formats — we refuse to decode MP3/M4A
/// inside the supervisor to keep the belt-and-suspenders check fast.
pub fn validate_spawn_only_artifacts(files: &[PathBuf]) -> Result<(), String> {
    for path in files {
        let metadata = match std::fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(_) => {
                return Err(format!(
                    "Skill reported success but artifact '{}' failed validation: missing",
                    path.display()
                ));
            }
        };
        let class = ArtifactMimeClass::from_path(path);
        let min_bytes = class.min_bytes();
        let size = metadata.len();
        if size < min_bytes {
            return Err(format!(
                "Skill reported success but artifact '{}' failed validation: size_{size}_below_{min_bytes}",
                path.display()
            ));
        }
        if matches!(class, ArtifactMimeClass::Audio) {
            validate_audio_content(path)?;
        }
    }
    Ok(())
}

/// Cheap, extension-aware audio content validation. Called after the size
/// floor has passed. Reads at most a few KB from disk per artifact.
fn validate_audio_content(path: &Path) -> Result<(), String> {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .unwrap_or_default();

    match extension.as_str() {
        "wav" => validate_wav_content(path),
        "mp3" => validate_simple_magic(path, &mp3_magic_check, "mp3_magic_missing"),
        "m4a" => validate_simple_magic(path, &m4a_magic_check, "m4a_ftyp_missing"),
        "ogg" => validate_simple_magic(path, &ogg_magic_check, "ogg_magic_missing"),
        "flac" => validate_simple_magic(path, &flac_magic_check, "flac_magic_missing"),
        // opus/aac are permitted without content checks — rare in our skills
        // and either container-wrapped (ogg) or hard to identify cheaply.
        _ => Ok(()),
    }
}

fn rejection(path: &Path, reason: &str) -> String {
    format!(
        "Skill reported success but artifact '{}' failed validation: {reason}",
        path.display()
    )
}

fn validate_simple_magic(
    path: &Path,
    check: &dyn Fn(&[u8]) -> bool,
    reason: &str,
) -> Result<(), String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(_) => return Err(rejection(path, "missing")),
    };
    if !check(&bytes) {
        return Err(rejection(path, reason));
    }
    Ok(())
}

fn mp3_magic_check(bytes: &[u8]) -> bool {
    if bytes.len() < 3 {
        return false;
    }
    // ID3v2 tagged
    if &bytes[0..3] == b"ID3" {
        return true;
    }
    // Raw MPEG audio frame sync: 0xFF followed by 0xFB (MPEG-1 Layer 3)
    // or 0xF3 (MPEG-2 Layer 3). Both are common for TTS output.
    if bytes[0] == 0xFF && (bytes[1] == 0xFB || bytes[1] == 0xF3 || bytes[1] == 0xF2) {
        return true;
    }
    false
}

fn m4a_magic_check(bytes: &[u8]) -> bool {
    bytes.len() >= 8 && &bytes[4..8] == b"ftyp"
}

fn ogg_magic_check(bytes: &[u8]) -> bool {
    bytes.len() >= 4 && &bytes[0..4] == b"OggS"
}

fn flac_magic_check(bytes: &[u8]) -> bool {
    bytes.len() >= 4 && &bytes[0..4] == b"fLaC"
}

/// Validate a WAV artifact's header, duration, and non-silence.
///
/// This does NOT parse every sub-chunk — we only need the format chunk's
/// sample-rate / channel / bits-per-sample fields and the data chunk's
/// length. The scan walks chunks linearly and bails on the first format
/// violation.
fn validate_wav_content(path: &Path) -> Result<(), String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(_) => return Err(rejection(path, "missing")),
    };
    if bytes.len() < 16 {
        return Err(rejection(path, "not_a_valid_wav_container"));
    }
    if &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" || &bytes[12..16] != b"fmt " {
        return Err(rejection(path, "not_a_valid_wav_container"));
    }

    // The fmt chunk starts at byte 12. Layout:
    //   bytes 12-15 : "fmt "
    //   bytes 16-19 : fmt chunk size (u32 LE, usually 16 for PCM)
    //   bytes 20-21 : format code (u16 LE, 1 = PCM)
    //   bytes 22-23 : num channels (u16 LE)
    //   bytes 24-27 : sample rate (u32 LE)
    //   bytes 32-33 : bits per sample (u16 LE)
    if bytes.len() < 36 {
        return Err(rejection(path, "not_a_valid_wav_container"));
    }
    let fmt_size = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]) as usize;
    let num_channels = u16::from_le_bytes([bytes[22], bytes[23]]) as usize;
    let sample_rate = u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]);
    let bits_per_sample = u16::from_le_bytes([bytes[34], bytes[35]]) as usize;

    if sample_rate == 0 || num_channels == 0 || bits_per_sample == 0 {
        return Err(rejection(path, "not_a_valid_wav_container"));
    }

    // Locate the data chunk. Subchunks begin after fmt + its payload.
    // fmt chunk header is bytes 12..20 (8 bytes), payload follows.
    let data_search_start = 20usize.saturating_add(fmt_size);
    let (data_offset, data_size) = match locate_data_chunk(&bytes, data_search_start) {
        Some(tuple) => tuple,
        None => return Err(rejection(path, "not_a_valid_wav_container")),
    };

    let bytes_per_sample_frame = num_channels.saturating_mul(bits_per_sample / 8).max(1);
    let num_sample_frames = data_size / bytes_per_sample_frame;
    let duration_secs = num_sample_frames as f64 / f64::from(sample_rate);
    if duration_secs < MIN_AUDIO_DURATION_SECS {
        let secs_ms = (duration_secs * 1000.0).round() as u64;
        let min_ms = (MIN_AUDIO_DURATION_SECS * 1000.0).round() as u64;
        return Err(rejection(
            path,
            &format!("duration_{secs_ms}ms_below_{min_ms}ms"),
        ));
    }

    // Silence check (16-bit PCM only). Other bit depths are treated as
    // non-silent by default — they are rare in our skills and we don't
    // want to introduce format-specific code paths here.
    if bits_per_sample == 16 {
        let payload_end = data_offset.saturating_add(data_size).min(bytes.len());
        let sample_window_end = data_offset
            .saturating_add(SILENCE_SAMPLE_BYTES)
            .min(payload_end);
        let payload = &bytes[data_offset..sample_window_end];
        if is_silent_pcm16(payload) {
            return Err(rejection(path, "audio_appears_to_be_silence"));
        }
    }

    Ok(())
}

/// Linear-scan the RIFF subchunks starting at `start` looking for "data".
/// Returns `(payload_offset, payload_size)` on success.
fn locate_data_chunk(bytes: &[u8], start: usize) -> Option<(usize, usize)> {
    let mut cursor = start;
    while cursor + 8 <= bytes.len() {
        let chunk_id = &bytes[cursor..cursor + 4];
        let chunk_size = u32::from_le_bytes([
            bytes[cursor + 4],
            bytes[cursor + 5],
            bytes[cursor + 6],
            bytes[cursor + 7],
        ]) as usize;
        let payload_offset = cursor + 8;
        if chunk_id == b"data" {
            return Some((payload_offset, chunk_size));
        }
        // Chunks are padded to even size per the RIFF spec.
        let advance = 8usize
            .saturating_add(chunk_size)
            .saturating_add(chunk_size & 1);
        if advance == 0 {
            return None;
        }
        cursor = cursor.saturating_add(advance);
    }
    None
}

/// Count 16-bit samples whose magnitude exceeds `SILENCE_SAMPLE_THRESHOLD`.
/// Returns `true` when the non-silent sample ratio is below the accepted
/// floor — i.e. the clip is effectively silent.
fn is_silent_pcm16(payload: &[u8]) -> bool {
    if payload.len() < 2 {
        return true;
    }
    let mut loud = 0usize;
    let mut total = 0usize;
    for chunk in payload.chunks_exact(2) {
        let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
        if sample.saturating_abs() > SILENCE_SAMPLE_THRESHOLD {
            loud += 1;
        }
        total += 1;
    }
    if total == 0 {
        return true;
    }
    (loud as f64 / total as f64) < MIN_NON_SILENT_SAMPLE_RATIO
}

impl std::fmt::Debug for TaskSupervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let progress_reporter_attached = self
            .progress_reporter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some();
        f.debug_struct("TaskSupervisor")
            .field("tasks", &self.tasks)
            .field("on_change", &"<callback>")
            .field("on_failure", &"<callback>")
            .field("progress_reporter", &progress_reporter_attached)
            .field(
                "persistence_path",
                &self
                    .persistence_path
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .map(|path| path.display().to_string()),
            )
            .finish()
    }
}

/// Human-readable label for a [`TaskRuntimeState`] used by the supervisor's
/// `ProgressReporter` bridge. The text is suffixed onto `<tool>: ` so the
/// chat UI can anchor a single bubble per tool_call_id and surface what the
/// background task is currently doing without inventing per-tool plumbing.
fn runtime_state_label(state: &TaskRuntimeState) -> &'static str {
    match state {
        TaskRuntimeState::Spawned => "spawned",
        TaskRuntimeState::ExecutingTool => "running",
        TaskRuntimeState::ResolvingOutputs => "resolving outputs",
        TaskRuntimeState::VerifyingOutputs => "verifying outputs",
        TaskRuntimeState::DeliveringOutputs => "delivering outputs",
        TaskRuntimeState::CleaningUp => "cleaning up",
        TaskRuntimeState::Completed => "completed",
        TaskRuntimeState::Failed => "failed",
    }
}

/// Supervisor that tracks background task lifecycle.
///
/// Thread-safe via interior `Mutex`. Cloning shares the same underlying state.
#[derive(Clone)]
pub struct TaskSupervisor {
    tasks: Arc<Mutex<HashMap<String, BackgroundTask>>>,
    on_change: Arc<Mutex<Option<OnChangeCallback>>>,
    on_failure: Arc<Mutex<Option<OnFailureCallback>>>,
    persistence_path: Arc<Mutex<Option<PathBuf>>>,
    /// Optional reporter that receives a [`ProgressEvent::ToolProgress`]
    /// for every supervised state transition. Wired by the agent's
    /// spawn_only branch so chat UIs can anchor progress strictly to the
    /// originating `tool_call_id` (the chat-bubble contract enforced by
    /// the SSE `tool_call_id` field on `tool_progress` frames).
    ///
    /// Synchronous tool calls never go through the supervisor, so this
    /// bridge naturally fires only on background-task transitions —
    /// there is no double-emission to worry about for the normal tool
    /// path that already reports its own ToolStarted/ToolCompleted.
    progress_reporter: Arc<Mutex<Option<Arc<dyn ProgressReporter>>>>,
}

impl Default for TaskSupervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskSupervisor {
    /// Create an empty supervisor.
    pub fn new() -> Self {
        Self {
            tasks: Arc::new(Mutex::new(HashMap::new())),
            on_change: Arc::new(Mutex::new(None)),
            on_failure: Arc::new(Mutex::new(None)),
            persistence_path: Arc::new(Mutex::new(None)),
            progress_reporter: Arc::new(Mutex::new(None)),
        }
    }

    /// Enable append-only persistence for task snapshots and restore existing state.
    pub fn enable_persistence(&self, path: impl Into<PathBuf>) -> std::io::Result<usize> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let ledger_path = path.display().to_string();
        let restored = Self::load_persisted_tasks(&path)?;
        {
            let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            for (task_id, task) in restored {
                match tasks.get(&task_id) {
                    Some(existing) if existing.updated_at >= task.updated_at => {}
                    _ => {
                        tasks.insert(task_id, task);
                    }
                }
            }
            for task in tasks.values_mut() {
                if task.task_ledger_path.as_deref() != Some(ledger_path.as_str()) {
                    task.task_ledger_path = Some(ledger_path.clone());
                }
            }
        }

        let mut guard = self
            .persistence_path
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *guard = Some(path);
        drop(guard);

        let snapshots: Vec<BackgroundTask> = {
            let tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            tasks.values().cloned().collect()
        };
        for task in snapshots {
            self.persist_snapshot(&task);
        }

        Ok(self.tasks.lock().unwrap_or_else(|e| e.into_inner()).len())
    }

    /// Set a callback that fires whenever a task's status changes.
    pub fn set_on_change(&self, cb: impl Fn(&BackgroundTask) + Send + Sync + 'static) {
        let mut guard = self.on_change.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(Box::new(cb));
    }

    /// Set a callback that fires only when a `spawn_only` task transitions to
    /// `Failed`. This is the M8.9 hook the session actor uses to enqueue a
    /// synthetic recovery turn. The callback is only invoked once per failed
    /// task — re-marking a task as failed (or any subsequent state change)
    /// will not re-fire the signal.
    pub fn set_on_failure_signal(
        &self,
        cb: impl Fn(&SpawnOnlyFailureSignal) + Send + Sync + 'static,
    ) {
        let mut guard = self.on_failure.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(Box::new(cb));
    }

    /// Attach a [`ProgressReporter`] that receives a
    /// [`ProgressEvent::ToolProgress`] for every supervised runtime-state
    /// transition. The emitted event carries the originating `tool_call_id`
    /// (`ProgressEvent::ToolProgress::tool_id`) so chat UIs can anchor every
    /// long-running spawn_only task to a single bubble — no per-tool plumbing
    /// required.
    ///
    /// Wired by the agent's spawn_only branch in `execution.rs`. Setting a
    /// reporter is idempotent; the latest reporter wins. Pass a
    /// [`crate::progress::SilentReporter`] to detach.
    pub fn set_progress_reporter(&self, reporter: Arc<dyn ProgressReporter>) {
        let mut guard = self
            .progress_reporter
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *guard = Some(reporter);
    }

    /// Emit a [`ProgressEvent::ToolProgress`] for `task` if a reporter has
    /// been wired via [`Self::set_progress_reporter`]. The message is
    /// `"<tool_name>: <state-label>"`, with the task's `error` text appended
    /// in parentheses on `Failed` transitions so the UI can surface the
    /// reason without re-walking the supervisor's state.
    fn emit_progress_for_state(&self, task: &BackgroundTask) {
        let guard = self
            .progress_reporter
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(reporter) = guard.as_ref().cloned() else {
            return;
        };
        drop(guard);
        let label = runtime_state_label(&task.runtime_state);
        let message = match task.runtime_state {
            TaskRuntimeState::Failed => match task.error.as_deref() {
                Some(reason) if !reason.is_empty() => {
                    format!("{}: {} ({})", task.tool_name, label, reason)
                }
                _ => format!("{}: {}", task.tool_name, label),
            },
            _ => format!("{}: {}", task.tool_name, label),
        };
        reporter.report(ProgressEvent::ToolProgress {
            name: task.tool_name.clone(),
            tool_id: task.tool_call_id.clone(),
            message,
        });
    }

    /// Register a new background task. Returns the generated task ID.
    pub fn register(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        session_key: Option<&str>,
    ) -> String {
        self.register_with_lineage(tool_name, tool_call_id, session_key, None)
    }

    /// Register a new background task with optional ledger-path lineage.
    pub fn register_with_lineage(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        session_key: Option<&str>,
        task_ledger_path: Option<&str>,
    ) -> String {
        self.register_full(tool_name, tool_call_id, session_key, task_ledger_path, None)
    }

    /// Register a new background task with optional ledger-path lineage and
    /// the original tool input. The tool input is preserved so failure
    /// signals can include it without re-walking the message history.
    pub fn register_with_input(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        session_key: Option<&str>,
        tool_input: Option<Value>,
    ) -> String {
        self.register_full(tool_name, tool_call_id, session_key, None, tool_input)
    }

    fn register_full(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        session_key: Option<&str>,
        task_ledger_path: Option<&str>,
        tool_input: Option<Value>,
    ) -> String {
        let id = TaskId::new().to_string();
        let derived_child_session_key = session_key.map(|parent| format!("{parent}#child-{id}"));
        let task = BackgroundTask {
            id: id.clone(),
            tool_name: tool_name.to_string(),
            tool_call_id: tool_call_id.to_string(),
            parent_session_key: session_key.map(|s| s.to_string()),
            child_session_key: derived_child_session_key,
            child_terminal_state: None,
            child_join_state: None,
            child_joined_at: None,
            child_failure_action: None,
            task_ledger_path: task_ledger_path.map(|path| path.to_string()).or_else(|| {
                self.persistence_path
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .map(|path| path.display().to_string())
            }),
            status: TaskStatus::Spawned,
            runtime_state: TaskRuntimeState::Spawned,
            runtime_detail: None,
            started_at: Utc::now(),
            updated_at: Utc::now(),
            completed_at: None,
            output_files: Vec::new(),
            error: None,
            session_key: session_key.map(|s| s.to_string()),
            tool_input,
        };
        let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        tasks.insert(id.clone(), task);
        drop(tasks);
        self.persist_snapshot_by_id(&id);
        record_child_session_lifecycle(
            "tracked",
            if session_key.is_some() {
                "registered"
            } else {
                "detached"
            },
        );
        id
    }

    /// Attach (or replace) the tool input for an already-registered task.
    /// Useful when the task is registered eagerly and the args become
    /// available later in the spawn pipeline.
    pub fn set_tool_input(&self, task_id: &str, tool_input: Value) {
        let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(task) = tasks.get_mut(task_id) {
            task.tool_input = Some(tool_input);
        }
    }

    /// Mark a task as running.
    pub fn mark_running(&self, task_id: &str) {
        let snapshot = {
            let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(task) = tasks.get_mut(task_id) {
                task.status = TaskStatus::Running;
                task.runtime_state = TaskRuntimeState::ExecutingTool;
                task.runtime_detail = None;
                task.updated_at = Utc::now();
                Some(task.clone())
            } else {
                None
            }
        };
        if let Some(ref task) = snapshot {
            self.persist_snapshot(task);
            self.notify_change(task);
            self.emit_progress_for_state(task);
        }
    }

    /// Update the fine-grained runtime state while keeping the coarse status.
    pub fn mark_runtime_state(
        &self,
        task_id: &str,
        runtime_state: TaskRuntimeState,
        runtime_detail: Option<String>,
    ) {
        let (snapshot, previous_detail) = {
            let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(task) = tasks.get_mut(task_id) {
                let previous_detail = task.runtime_detail.clone();
                task.runtime_state = runtime_state;
                task.runtime_detail = runtime_detail;
                task.updated_at = Utc::now();
                (Some(task.clone()), previous_detail)
            } else {
                (None, None)
            }
        };
        if let Some(ref task) = snapshot {
            self.persist_snapshot(task);
            self.notify_change(task);
            self.emit_progress_for_state(task);
            let (previous_kind, previous_phase) = workflow_labels(previous_detail.as_deref());
            let (current_kind, current_phase) = workflow_labels(task.runtime_detail.as_deref());
            if let (Some(workflow_kind), Some(to_phase)) =
                (current_kind.as_deref(), current_phase.as_deref())
            {
                let from_phase = if previous_kind.as_deref() == Some(workflow_kind) {
                    previous_phase.as_deref().unwrap_or("untracked")
                } else {
                    "untracked"
                };
                if from_phase != to_phase {
                    record_workflow_phase_transition(workflow_kind, from_phase, to_phase);
                }
            }
        }
    }

    /// Mark a task as completed with output files.
    pub fn mark_completed(&self, task_id: &str, output_files: Vec<String>) {
        let snapshot = {
            let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(task) = tasks.get_mut(task_id) {
                task.status = TaskStatus::Completed;
                task.runtime_state = TaskRuntimeState::Completed;
                task.updated_at = Utc::now();
                task.completed_at = Some(Utc::now());
                task.output_files = output_files;
                Some(task.clone())
            } else {
                None
            }
        };
        if let Some(ref task) = snapshot {
            self.persist_snapshot(task);
            self.notify_change(task);
            self.emit_progress_for_state(task);
        }
    }

    /// Mark a task as completed only if every reported artifact passes
    /// MIME-class size validation. Otherwise mark the task failed with a
    /// structured validation error. Tasks with an empty `output_files`
    /// list pass through to the normal `mark_completed` path — the
    /// "no artifacts" case is the workspace contract layer's concern.
    ///
    /// Returns `Ok(())` when the task transitions to `Completed`, or
    /// `Err(reason)` when validation rejected the artifacts and the task
    /// was transitioned to `Failed` instead. The error string matches the
    /// value stored on the task's `error` field so callers can propagate
    /// it verbatim into the session notification.
    pub fn mark_completed_with_validation(
        &self,
        task_id: &str,
        output_files: Vec<String>,
    ) -> Result<(), String> {
        let paths: Vec<PathBuf> = output_files.iter().map(PathBuf::from).collect();
        match validate_spawn_only_artifacts(&paths) {
            Ok(()) => {
                self.mark_completed(task_id, output_files);
                Ok(())
            }
            Err(error) => {
                self.mark_failed(task_id, error.clone());
                Err(error)
            }
        }
    }

    /// Mark a task as failed with an error message.
    ///
    /// On the FIRST transition from a non-`Failed` status to `Failed`, also
    /// emits a `SpawnOnlyFailureSignal` so listeners (e.g. the session
    /// actor) can schedule a recovery turn. Re-marking an already-failed
    /// task is a no-op for the failure signal — this guarantees at most one
    /// recovery attempt per task even if multiple paths report the failure.
    pub fn mark_failed(&self, task_id: &str, error: String) {
        let (snapshot, was_already_failed) = {
            let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(task) = tasks.get_mut(task_id) {
                let already_failed = task.status == TaskStatus::Failed;
                task.status = TaskStatus::Failed;
                task.runtime_state = TaskRuntimeState::Failed;
                task.updated_at = Utc::now();
                task.completed_at = Some(Utc::now());
                task.error = Some(error);
                (Some(task.clone()), already_failed)
            } else {
                (None, false)
            }
        };
        if let Some(ref task) = snapshot {
            self.persist_snapshot(task);
            self.notify_change(task);
            if !was_already_failed {
                self.emit_progress_for_state(task);
                self.notify_failure(task);
            }
        }
    }

    /// Emit a `SpawnOnlyFailureSignal` for a freshly-failed task, if a
    /// failure callback has been registered. The error_message is taken
    /// from the task's `error` field (set immediately before this call).
    fn notify_failure(&self, task: &BackgroundTask) {
        let guard = self.on_failure.lock().unwrap_or_else(|e| e.into_inner());
        let Some(cb) = guard.as_ref() else {
            return;
        };
        let error_message = task.error.clone().unwrap_or_default();
        let suggested_alternatives = parse_alternatives(&error_message);
        let signal = SpawnOnlyFailureSignal {
            task_id: task.id.clone(),
            tool_name: task.tool_name.clone(),
            tool_input: task.tool_input.clone().unwrap_or(Value::Null),
            error_message,
            suggested_alternatives,
            parent_session_key: task.parent_session_key.clone(),
        };
        cb(&signal);
    }

    /// Record the child-session contract outcome for a task.
    pub fn mark_child_session_outcome(
        &self,
        task_id: &str,
        terminal_state: ChildSessionTerminalState,
        join_state: ChildSessionJoinState,
    ) {
        let failure_action = child_failure_action_for_terminal_state(&terminal_state);
        let kind_label = child_terminal_kind_label(&terminal_state);
        let outcome_label = child_join_outcome_label(&join_state);
        let snapshot = {
            let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(task) = tasks.get_mut(task_id) {
                task.child_terminal_state = Some(terminal_state);
                task.child_join_state = Some(join_state.clone());
                task.child_joined_at = match join_state {
                    ChildSessionJoinState::Joined => Some(Utc::now()),
                    ChildSessionJoinState::Orphaned => None,
                };
                task.child_failure_action = failure_action;
                task.updated_at = Utc::now();
                Some(task.clone())
            } else {
                None
            }
        };
        if let Some(ref task) = snapshot {
            self.persist_snapshot(task);
            self.notify_change(task);
            record_child_session_lifecycle(kind_label, outcome_label);
            if matches!(join_state, ChildSessionJoinState::Orphaned) {
                record_child_session_orphan("terminal_event_not_joined");
            }
        }
    }

    /// Apply a structured harness event to a tracked task.
    pub fn apply_harness_event(
        &self,
        task_id: &str,
        event: &HarnessEvent,
    ) -> Result<(), &'static str> {
        let snapshot = self.get_task(task_id).ok_or("unknown task")?;
        let (workflow_kind, current_phase) = workflow_labels(snapshot.runtime_detail.as_deref());
        let runtime_detail =
            event.runtime_detail_value(workflow_kind.as_deref(), current_phase.as_deref());

        match &event.payload {
            HarnessEventPayload::Progress { .. }
            | HarnessEventPayload::Phase { .. }
            | HarnessEventPayload::Retry { .. } => {
                self.mark_runtime_state(
                    task_id,
                    TaskRuntimeState::ExecutingTool,
                    Some(runtime_detail.to_string()),
                );
            }
            HarnessEventPayload::Artifact { .. } => {
                self.mark_runtime_state(
                    task_id,
                    TaskRuntimeState::DeliveringOutputs,
                    Some(runtime_detail.to_string()),
                );
            }
            HarnessEventPayload::ValidatorResult { data } => {
                self.mark_runtime_state(
                    task_id,
                    TaskRuntimeState::VerifyingOutputs,
                    Some(runtime_detail.to_string()),
                );
                if !data.passed {
                    let message = data.message.clone().unwrap_or_else(|| {
                        "validator rejected structured harness event".to_string()
                    });
                    self.mark_failed(task_id, message);
                }
            }
            HarnessEventPayload::Failure { data } => {
                self.mark_runtime_state(
                    task_id,
                    TaskRuntimeState::Failed,
                    Some(runtime_detail.to_string()),
                );
                self.mark_failed(task_id, data.message.clone());
            }
            HarnessEventPayload::McpServerCall { .. } => {
                // MCP-server dispatch events are audit records — they describe
                // a call that already mapped onto the supervisor via
                // run-to-completion. Nothing to reapply to lifecycle state.
            }
            HarnessEventPayload::SubAgentDispatch { .. } => {
                // Dispatch events are observational — they record the fact
                // that a task was shipped off to an MCP-backed sub-agent
                // without mutating the task's terminal state. The outer
                // spawn lifecycle still decides when the task completes or
                // fails; we just attach the structured detail so operators
                // can see which backend is servicing the task.
                self.mark_runtime_state(
                    task_id,
                    TaskRuntimeState::ExecutingTool,
                    Some(runtime_detail.to_string()),
                );
            }
            HarnessEventPayload::SwarmDispatch { .. } => {
                // Swarm dispatch events are observational from the
                // supervisor's perspective — the `octos-swarm` primitive
                // owns its own redb-backed session state and drives the
                // retry loop. We just surface the aggregate detail so
                // operators can see fan-out progress.
                self.mark_runtime_state(
                    task_id,
                    TaskRuntimeState::ExecutingTool,
                    Some(runtime_detail.to_string()),
                );
            }
            HarnessEventPayload::SwarmReviewDecision { .. } => {
                // Review decisions are supervisor-authored audit records.
                // They do not move the task lifecycle — the originating
                // dispatch already reached a terminal state when the
                // review panel was shown. Surface the detail so operators
                // can see accept/reject transitions on the timeline.
                self.mark_runtime_state(
                    task_id,
                    snapshot.runtime_state,
                    Some(runtime_detail.to_string()),
                );
            }
            HarnessEventPayload::CostAttribution { .. } => {
                // Cost attributions are purely observational — they are
                // committed after a sub-agent dispatch succeeds and do
                // not move the task's lifecycle. Attach the structured
                // detail so operators see the spend breakdown on the
                // same task row as the dispatch.
                self.mark_runtime_state(
                    task_id,
                    TaskRuntimeState::ExecutingTool,
                    Some(runtime_detail.to_string()),
                );
            }
            HarnessEventPayload::RoutingDecision { .. } => {
                // Routing decisions are observational — they do not change the
                // task's lifecycle state. We still attach the detail so the
                // operator dashboard can surface the tier/reasons for this
                // turn without inventing a dedicated sidecar channel.
                self.mark_runtime_state(
                    task_id,
                    TaskRuntimeState::ExecutingTool,
                    Some(runtime_detail.to_string()),
                );
            }
            HarnessEventPayload::CredentialRotation { .. } => {
                // Credential rotations are observability-only — they do not
                // change the task lifecycle. We still update runtime_detail
                // so operators can see which key is now active.
                self.mark_runtime_state(
                    task_id,
                    snapshot.runtime_state,
                    Some(runtime_detail.to_string()),
                );
            }
            HarnessEventPayload::SessionSanitized { .. } => {
                // Session-sanitize events are observability-only (M8.6).
                // They fire once per resume and describe what the resume
                // policy dropped — the task lifecycle is not affected; the
                // session actor will subsequently drive normal
                // Queued → Executing transitions as usual.
                self.mark_runtime_state(
                    task_id,
                    snapshot.runtime_state,
                    Some(runtime_detail.to_string()),
                );
            }
            HarnessEventPayload::SubagentProgress { .. } => {
                // Sub-agent progress is a periodic textual summary generated
                // by `AgentSummaryGenerator`. It does not change the
                // lifecycle state — we simply fold it into the runtime
                // detail so dashboards can render a live "what is the
                // sub-agent doing" label.
                self.mark_runtime_state(
                    task_id,
                    snapshot.runtime_state,
                    Some(runtime_detail.to_string()),
                );
            }
            HarnessEventPayload::Error { data } => {
                // Structured error events are diagnostic — record them in the
                // runtime detail but only transition to Failed when the
                // recovery hint marks the variant as non-retryable.
                self.mark_runtime_state(
                    task_id,
                    TaskRuntimeState::ExecutingTool,
                    Some(runtime_detail.to_string()),
                );
                if matches!(data.recovery.as_str(), "fail_fast" | "bug") {
                    self.mark_failed(task_id, data.message.clone());
                }
            }
        }

        Ok(())
    }

    fn persist_snapshot_by_id(&self, task_id: &str) {
        let snapshot = {
            let tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            tasks.get(task_id).cloned()
        };
        if let Some(task) = snapshot {
            self.persist_snapshot(&task);
        }
    }

    fn persist_snapshot(&self, task: &BackgroundTask) {
        let Some(path) = self
            .persistence_path
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        else {
            return;
        };

        let record = PersistedTaskRecord {
            schema_version: CURRENT_TASK_LEDGER_SCHEMA,
            task: task.clone(),
        };
        let Ok(json) = serde_json::to_string(&record) else {
            return;
        };

        if let Err(error) = Self::append_persisted_task(&path, &json) {
            tracing::warn!(
                task_id = %task.id,
                path = %path.display(),
                error = %error,
                "failed to persist background task snapshot"
            );
        }
    }

    /// Return a snapshot for a specific task id.
    pub fn get_task(&self, task_id: &str) -> Option<BackgroundTask> {
        let tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        tasks.get(task_id).cloned()
    }

    /// Return the persistence path for task snapshots, if enabled.
    pub fn persistence_path(&self) -> Option<PathBuf> {
        self.persistence_path
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn append_persisted_task(path: &PathBuf, json: &str) -> std::io::Result<()> {
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(file, "{json}")?;
        Ok(())
    }

    fn load_persisted_tasks(path: &PathBuf) -> std::io::Result<HashMap<String, BackgroundTask>> {
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(HashMap::new());
            }
            Err(error) => return Err(error),
        };

        let mut restored = HashMap::new();
        for line in BufReader::new(file).lines() {
            let Ok(line) = line else {
                continue;
            };
            if line.trim().is_empty() {
                continue;
            }
            let Ok(record) = serde_json::from_str::<PersistedTaskRecord>(&line) else {
                continue;
            };
            if record.schema_version > CURRENT_TASK_LEDGER_SCHEMA {
                continue;
            }
            restored.insert(record.task.id.clone(), record.task);
        }
        Ok(restored)
    }

    /// Fire the on_change callback (if set) with a task snapshot.
    fn notify_change(&self, task: &BackgroundTask) {
        let guard = self.on_change.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ref cb) = *guard {
            cb(task);
        }
    }

    /// Return all non-completed (active) tasks.
    pub fn get_active_tasks(&self) -> Vec<BackgroundTask> {
        let tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        tasks
            .values()
            .filter(|t| t.status.is_active())
            .cloned()
            .collect()
    }

    /// Return all tracked tasks.
    pub fn get_all_tasks(&self) -> Vec<BackgroundTask> {
        let tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        tasks.values().cloned().collect()
    }

    /// Return all tasks belonging to a specific session.
    pub fn get_tasks_for_session(&self, session_key: &str) -> Vec<BackgroundTask> {
        let tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        tasks
            .values()
            .filter(|t| t.session_key.as_deref() == Some(session_key))
            .cloned()
            .collect()
    }

    /// Number of active (non-completed, non-failed) tasks.
    pub fn task_count(&self) -> usize {
        let tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        tasks.values().filter(|t| t.status.is_active()).count()
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal-but-valid mono 16-bit PCM WAV containing a sine tone.
    /// `duration_secs` controls the payload length; `sample_rate` is Hz.
    /// Setting `silent` to `true` emits zero-valued samples so silence-check
    /// tests can exercise the non-silent PCM gate.
    fn build_sine_wav(duration_secs: f64, sample_rate: u32, silent: bool) -> Vec<u8> {
        let num_samples = (duration_secs * f64::from(sample_rate)) as u32;
        let bits_per_sample: u16 = 16;
        let num_channels: u16 = 1;
        let byte_rate = sample_rate * u32::from(num_channels) * u32::from(bits_per_sample) / 8;
        let block_align = num_channels * bits_per_sample / 8;
        let data_size = num_samples * u32::from(block_align);
        let file_size = 36 + data_size;

        let mut out = Vec::with_capacity(44 + data_size as usize);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&file_size.to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
        out.extend_from_slice(&1u16.to_le_bytes()); // format = PCM
        out.extend_from_slice(&num_channels.to_le_bytes());
        out.extend_from_slice(&sample_rate.to_le_bytes());
        out.extend_from_slice(&byte_rate.to_le_bytes());
        out.extend_from_slice(&block_align.to_le_bytes());
        out.extend_from_slice(&bits_per_sample.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_size.to_le_bytes());

        if silent {
            out.resize(out.len() + data_size as usize, 0);
        } else {
            // 440 Hz sine, 0.5 amplitude — safely above the 256 silence floor.
            let amplitude = 16_000.0_f64;
            let frequency = 440.0_f64;
            for n in 0..num_samples {
                let t = f64::from(n) / f64::from(sample_rate);
                let sample =
                    (amplitude * (2.0 * std::f64::consts::PI * frequency * t).sin()) as i16;
                out.extend_from_slice(&sample.to_le_bytes());
            }
        }
        out
    }

    /// A tiny MP3-like byte sequence starting with a valid ID3v2 header.
    /// We do not decode — only the 3-byte magic is inspected.
    fn build_id3_tagged_mp3(len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        out.extend_from_slice(b"ID3\x03\x00\x00\x00\x00\x00\x00");
        out.resize(len, 0);
        out
    }

    /// A tiny MP3-like byte sequence starting with an MPEG frame-sync marker.
    fn build_mpeg_sync_mp3(len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        // 0xFF 0xFB => MPEG-1 Layer 3, no CRC
        out.extend_from_slice(&[0xFFu8, 0xFB, 0x90, 0x00]);
        out.resize(len, 0);
        out
    }

    /// A tiny M4A-like byte sequence: 4 bytes of size then `ftyp` marker.
    fn build_m4a(len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x20]); // size
        out.extend_from_slice(b"ftyp");
        out.extend_from_slice(b"M4A ");
        out.extend_from_slice(&[0x00; 8]);
        out.resize(len, 0);
        out
    }

    /// Minimal OGG-like page starting with `OggS`.
    fn build_ogg(len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        out.extend_from_slice(b"OggS");
        out.resize(len, 0);
        out
    }

    /// Minimal FLAC-like stream starting with `fLaC`.
    fn build_flac(len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        out.extend_from_slice(b"fLaC");
        out.resize(len, 0);
        out
    }

    #[test]
    fn should_register_task_with_spawned_status() {
        let supervisor = TaskSupervisor::new();
        let id = supervisor.register("tts", "call-123", None);

        let tasks = supervisor.get_all_tasks();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, id);
        assert_eq!(tasks[0].tool_name, "tts");
        assert_eq!(tasks[0].tool_call_id, "call-123");
        assert_eq!(tasks[0].status, TaskStatus::Spawned);
        assert_eq!(tasks[0].runtime_state, TaskRuntimeState::Spawned);
        assert!(tasks[0].child_terminal_state.is_none());
        assert!(tasks[0].child_join_state.is_none());
        assert!(tasks[0].child_failure_action.is_none());
        assert!(tasks[0].completed_at.is_none());
        assert!(tasks[0].updated_at >= tasks[0].started_at);
    }

    #[test]
    fn should_register_task_with_lineage_and_ledger_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let ledger_path = dir.path().join("tasks.jsonl");

        let supervisor = TaskSupervisor::new();
        supervisor.enable_persistence(&ledger_path).unwrap();

        let id = supervisor.register_with_lineage(
            "podcast_generate",
            "call-42",
            Some("api:session"),
            Some(ledger_path.to_str().unwrap()),
        );

        let task = supervisor.get_task(&id).expect("task missing");
        let expected_child = format!("api:session#child-{id}");
        assert_eq!(task.parent_session_key.as_deref(), Some("api:session"));
        assert_eq!(
            task.child_session_key.as_deref(),
            Some(expected_child.as_str())
        );
        assert_eq!(
            task.task_ledger_path.as_deref(),
            Some(ledger_path.to_str().unwrap())
        );
    }

    #[test]
    fn should_transition_through_lifecycle_states() {
        let supervisor = TaskSupervisor::new();
        let id = supervisor.register("tts", "call-1", None);
        let task = &supervisor.get_all_tasks()[0];
        assert_eq!(task.lifecycle_state(), TaskLifecycleState::Queued);

        supervisor.mark_running(&id);
        let task = &supervisor.get_all_tasks()[0];
        assert_eq!(task.status, TaskStatus::Running);
        assert_eq!(task.runtime_state, TaskRuntimeState::ExecutingTool);
        assert_eq!(task.lifecycle_state(), TaskLifecycleState::Running);

        supervisor.mark_runtime_state(
            &id,
            TaskRuntimeState::DeliveringOutputs,
            Some("send_file".to_string()),
        );
        let task = &supervisor.get_all_tasks()[0];
        assert_eq!(task.status, TaskStatus::Running);
        assert_eq!(task.runtime_state, TaskRuntimeState::DeliveringOutputs);
        assert_eq!(task.runtime_detail.as_deref(), Some("send_file"));
        assert_eq!(task.lifecycle_state(), TaskLifecycleState::Verifying);

        supervisor.mark_completed(&id, vec!["output.mp3".to_string()]);
        let task = &supervisor.get_all_tasks()[0];
        assert_eq!(task.status, TaskStatus::Completed);
        assert_eq!(task.runtime_state, TaskRuntimeState::Completed);
        assert_eq!(task.lifecycle_state(), TaskLifecycleState::Ready);
        assert!(task.completed_at.is_some());
        assert_eq!(task.output_files, vec!["output.mp3"]);
    }

    #[test]
    fn should_apply_harness_progress_event_and_notify() {
        let supervisor = TaskSupervisor::new();
        let id = supervisor.register("deep_search", "call-9", Some("api:session"));
        supervisor.mark_running(&id);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        supervisor.set_on_change(move |task| {
            let _ = tx.send(task.clone());
        });

        let event = crate::harness_events::HarnessEvent::progress(
            "api:session",
            id.clone(),
            Some("deep_research"),
            "fetching_sources",
            Some("Fetching source 3/12"),
            Some(0.42),
        );

        supervisor.apply_harness_event(&id, &event).unwrap();

        let task = supervisor.get_task(&id).expect("task missing");
        let detail: serde_json::Value =
            serde_json::from_str(task.runtime_detail.as_deref().unwrap()).unwrap();
        assert_eq!(detail["workflow_kind"], "deep_research");
        assert_eq!(detail["current_phase"], "fetching_sources");
        assert_eq!(detail["progress_message"], "Fetching source 3/12");
        let progress = detail["progress"].as_f64().unwrap();
        assert!((progress - 0.42).abs() < 0.0001);

        let notified = rx.try_recv().expect("callback should fire");
        let notified_detail: serde_json::Value =
            serde_json::from_str(notified.runtime_detail.as_deref().unwrap()).unwrap();
        assert_eq!(notified_detail["current_phase"], "fetching_sources");
        assert_eq!(notified.lifecycle_state(), TaskLifecycleState::Running);
    }

    #[test]
    fn should_persist_harness_progress_event_for_replay() {
        let dir = tempfile::TempDir::new().unwrap();
        let ledger_path = dir.path().join("tasks.jsonl");

        let supervisor = TaskSupervisor::new();
        supervisor.enable_persistence(&ledger_path).unwrap();
        let id =
            supervisor.register_with_lineage("deep_search", "call-9", Some("api:session"), None);
        supervisor.mark_running(&id);

        let event = crate::harness_events::HarnessEvent::progress(
            "api:session",
            id.clone(),
            Some("deep_research"),
            "fetch",
            Some("Fetching 4 pages"),
            Some(0.4),
        );
        supervisor.apply_harness_event(&id, &event).unwrap();

        let restored = TaskSupervisor::new();
        restored.enable_persistence(&ledger_path).unwrap();
        let task = restored.get_task(&id).expect("restored task missing");
        let detail: serde_json::Value =
            serde_json::from_str(task.runtime_detail.as_deref().unwrap()).unwrap();
        assert_eq!(
            detail["schema"],
            crate::harness_events::HARNESS_EVENT_SCHEMA_V1
        );
        assert_eq!(detail["session_id"], "api:session");
        assert_eq!(detail["task_id"], id);
        assert_eq!(detail["workflow_kind"], "deep_research");
        assert_eq!(detail["current_phase"], "fetch");
        assert_eq!(detail["progress_message"], "Fetching 4 pages");
        assert_eq!(task.status, TaskStatus::Running);
    }

    #[test]
    fn should_persist_child_session_outcome_state() {
        let supervisor = TaskSupervisor::new();
        let id = supervisor.register("tts", "call-7", Some("api:session"));

        supervisor.mark_child_session_outcome(
            &id,
            ChildSessionTerminalState::RetryableFailure,
            ChildSessionJoinState::Joined,
        );

        let task = supervisor.get_task(&id).expect("task missing");
        assert_eq!(
            task.child_terminal_state,
            Some(ChildSessionTerminalState::RetryableFailure)
        );
        assert_eq!(task.child_join_state, Some(ChildSessionJoinState::Joined));
        assert_eq!(
            task.child_failure_action,
            Some(ChildSessionFailureAction::Retry)
        );
        assert!(task.child_joined_at.is_some());
    }

    #[test]
    fn should_track_failed_tasks_with_error() {
        let supervisor = TaskSupervisor::new();
        let id = supervisor.register("tts", "call-2", None);

        supervisor.mark_running(&id);
        supervisor.mark_failed(&id, "connection refused".to_string());

        let task = &supervisor.get_all_tasks()[0];
        assert_eq!(task.status, TaskStatus::Failed);
        assert_eq!(task.runtime_state, TaskRuntimeState::Failed);
        assert_eq!(task.lifecycle_state(), TaskLifecycleState::Failed);
        assert_eq!(task.error.as_deref(), Some("connection refused"));
        assert!(task.completed_at.is_some());
    }

    #[test]
    fn should_count_only_active_tasks() {
        let supervisor = TaskSupervisor::new();
        let id1 = supervisor.register("tts", "call-1", None);
        let id2 = supervisor.register("tts", "call-2", None);
        let _id3 = supervisor.register("tts", "call-3", None);

        assert_eq!(supervisor.task_count(), 3);

        supervisor.mark_completed(&id1, vec![]);
        assert_eq!(supervisor.task_count(), 2);

        supervisor.mark_failed(&id2, "err".to_string());
        assert_eq!(supervisor.task_count(), 1);
    }

    #[test]
    fn should_return_only_active_tasks_in_get_active() {
        let supervisor = TaskSupervisor::new();
        let id1 = supervisor.register("tts", "call-1", None);
        let _id2 = supervisor.register("tts", "call-2", None);

        supervisor.mark_completed(&id1, vec![]);

        let active = supervisor.get_active_tasks();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].tool_call_id, "call-2");
    }

    #[test]
    fn should_be_empty_when_new() {
        let supervisor = TaskSupervisor::new();
        assert_eq!(supervisor.task_count(), 0);
        assert!(supervisor.get_all_tasks().is_empty());
        assert!(supervisor.get_active_tasks().is_empty());
    }

    #[test]
    fn should_ignore_unknown_task_ids() {
        let supervisor = TaskSupervisor::new();
        // These should not panic
        supervisor.mark_running("nonexistent");
        supervisor.mark_completed("nonexistent", vec![]);
        supervisor.mark_failed("nonexistent", "err".to_string());
        assert_eq!(supervisor.task_count(), 0);
    }

    #[test]
    fn should_restore_running_task_state_after_restart() {
        let dir = tempfile::TempDir::new().unwrap();
        let ledger_path = dir.path().join("tasks.jsonl");

        let supervisor = TaskSupervisor::new();
        supervisor.enable_persistence(&ledger_path).unwrap();

        let task_id =
            supervisor.register_with_lineage("deep_search", "call-1", Some("api:session"), None);
        supervisor.mark_running(&task_id);
        supervisor.mark_runtime_state(
            &task_id,
            TaskRuntimeState::ResolvingOutputs,
            Some("collecting evidence".to_string()),
        );

        let restored = TaskSupervisor::new();
        restored.enable_persistence(&ledger_path).unwrap();

        let tasks = restored.get_all_tasks();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, task_id);
        assert_eq!(tasks[0].status, TaskStatus::Running);
        assert_eq!(tasks[0].runtime_state, TaskRuntimeState::ResolvingOutputs);
        assert_eq!(
            tasks[0].runtime_detail.as_deref(),
            Some("collecting evidence")
        );
        let expected_child = format!("api:session#child-{task_id}");
        assert_eq!(tasks[0].parent_session_key.as_deref(), Some("api:session"));
        assert_eq!(
            tasks[0].child_session_key.as_deref(),
            Some(expected_child.as_str())
        );
        assert_eq!(
            tasks[0].task_ledger_path.as_deref(),
            Some(ledger_path.to_str().unwrap())
        );
    }

    #[test]
    fn should_restore_completed_and_failed_truth_after_restart() {
        let dir = tempfile::TempDir::new().unwrap();
        let ledger_path = dir.path().join("tasks.jsonl");

        let supervisor = TaskSupervisor::new();
        supervisor.enable_persistence(&ledger_path).unwrap();

        let completed =
            supervisor.register_with_lineage("fm_tts", "call-2", Some("api:session"), None);
        supervisor.mark_running(&completed);
        supervisor.mark_runtime_state(
            &completed,
            TaskRuntimeState::DeliveringOutputs,
            Some("send_file".to_string()),
        );
        supervisor.mark_completed(&completed, vec!["/tmp/output.mp3".to_string()]);
        supervisor.mark_child_session_outcome(
            &completed,
            ChildSessionTerminalState::Completed,
            ChildSessionJoinState::Joined,
        );

        let failed = supervisor.register_with_lineage(
            "podcast_generate",
            "call-3",
            Some("api:session"),
            None,
        );
        supervisor.mark_running(&failed);
        supervisor.mark_failed(&failed, "No dialogue lines found in script".to_string());
        supervisor.mark_child_session_outcome(
            &failed,
            ChildSessionTerminalState::TerminalFailure,
            ChildSessionJoinState::Orphaned,
        );

        let restored = TaskSupervisor::new();
        restored.enable_persistence(&ledger_path).unwrap();

        let tasks = restored.get_all_tasks();
        assert_eq!(tasks.len(), 2);

        let completed_task = tasks
            .iter()
            .find(|task| task.id == completed)
            .expect("completed task missing");
        assert_eq!(completed_task.status, TaskStatus::Completed);
        assert_eq!(completed_task.runtime_state, TaskRuntimeState::Completed);
        assert_eq!(completed_task.runtime_detail.as_deref(), Some("send_file"));
        assert_eq!(completed_task.output_files, vec!["/tmp/output.mp3"]);
        let expected_completed_child = format!("api:session#child-{completed}");
        assert_eq!(
            completed_task.parent_session_key.as_deref(),
            Some("api:session")
        );
        assert_eq!(
            completed_task.child_session_key.as_deref(),
            Some(expected_completed_child.as_str())
        );
        assert_eq!(
            completed_task.task_ledger_path.as_deref(),
            Some(ledger_path.to_str().unwrap())
        );
        assert_eq!(
            completed_task.child_terminal_state,
            Some(ChildSessionTerminalState::Completed)
        );
        assert_eq!(
            completed_task.child_join_state,
            Some(ChildSessionJoinState::Joined)
        );
        assert_eq!(completed_task.child_failure_action, None);
        assert!(completed_task.child_joined_at.is_some());

        let failed_task = tasks
            .iter()
            .find(|task| task.id == failed)
            .expect("failed task missing");
        assert_eq!(failed_task.status, TaskStatus::Failed);
        assert_eq!(failed_task.runtime_state, TaskRuntimeState::Failed);
        assert_eq!(failed_task.runtime_detail, None);
        assert_eq!(
            failed_task.error.as_deref(),
            Some("No dialogue lines found in script")
        );
        assert_eq!(
            failed_task.parent_session_key.as_deref(),
            Some("api:session")
        );
        let expected_failed_child = format!("api:session#child-{failed}");
        assert_eq!(
            failed_task.child_session_key.as_deref(),
            Some(expected_failed_child.as_str())
        );
        assert_eq!(
            failed_task.task_ledger_path.as_deref(),
            Some(ledger_path.to_str().unwrap())
        );
        assert_eq!(
            failed_task.child_terminal_state,
            Some(ChildSessionTerminalState::TerminalFailure)
        );
        assert_eq!(
            failed_task.child_join_state,
            Some(ChildSessionJoinState::Orphaned)
        );
        assert_eq!(
            failed_task.child_failure_action,
            Some(ChildSessionFailureAction::Escalate)
        );
        assert!(failed_task.child_joined_at.is_none());
    }

    #[test]
    fn should_mark_task_failed_when_audio_artifact_below_1kb() {
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("voice.wav");
        std::fs::write(&stub, vec![0u8; 44]).unwrap();

        let supervisor = TaskSupervisor::new();
        let id = supervisor.register("fm_tts", "call-1", None);
        supervisor.mark_running(&id);

        let result = supervisor
            .mark_completed_with_validation(&id, vec![stub.to_string_lossy().to_string()]);

        let error = result.expect_err("undersized audio must fail validation");
        assert!(
            error.contains("voice.wav"),
            "error should mention the failing path: {error}"
        );
        assert!(
            error.contains("size_44_below_1024"),
            "error should carry structured size detail: {error}"
        );

        let task = supervisor.get_task(&id).expect("task missing");
        assert_eq!(task.status, TaskStatus::Failed);
        assert_eq!(task.runtime_state, TaskRuntimeState::Failed);
        assert_eq!(task.error.as_deref(), Some(error.as_str()));
    }

    #[test]
    fn should_accept_audio_artifact_at_or_above_1kb() {
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("voice.mp3");
        // Valid ID3-tagged MP3 padded to 1 KB. Size floor AND magic number
        // both satisfied — this is the belt-and-suspenders happy path.
        std::fs::write(&clip, build_id3_tagged_mp3(1024)).unwrap();

        let supervisor = TaskSupervisor::new();
        let id = supervisor.register("fm_tts", "call-2", None);
        supervisor.mark_running(&id);

        supervisor
            .mark_completed_with_validation(&id, vec![clip.to_string_lossy().to_string()])
            .expect("1KB audio should satisfy the contract");

        let task = supervisor.get_task(&id).expect("task missing");
        assert_eq!(task.status, TaskStatus::Completed);
        assert_eq!(task.runtime_state, TaskRuntimeState::Completed);
        assert!(task.error.is_none());
    }

    #[test]
    fn should_mark_task_failed_when_artifact_path_is_missing() {
        let supervisor = TaskSupervisor::new();
        let id = supervisor.register("fm_tts", "call-3", None);
        supervisor.mark_running(&id);

        let result =
            supervisor.mark_completed_with_validation(&id, vec!["/nonexistent/voice.wav".into()]);

        let error = result.expect_err("missing artifact must fail validation");
        assert!(error.contains("/nonexistent/voice.wav"));
        assert!(error.contains("missing"));

        let task = supervisor.get_task(&id).expect("task missing");
        assert_eq!(task.status, TaskStatus::Failed);
        assert_eq!(task.error.as_deref(), Some(error.as_str()));
    }

    #[test]
    fn should_preserve_completed_status_when_all_artifacts_pass() {
        let dir = tempfile::tempdir().unwrap();
        let audio = dir.path().join("clip.wav");
        let image = dir.path().join("cover.png");
        let video = dir.path().join("trailer.mp4");
        let report = dir.path().join("summary.txt");
        // 1 s, 16 kHz, 16-bit mono sine — passes WAV header + duration + silence.
        std::fs::write(&audio, build_sine_wav(1.0, 16_000, false)).unwrap();
        std::fs::write(&image, vec![0u8; 1024]).unwrap();
        std::fs::write(&video, vec![0u8; 16_384]).unwrap();
        std::fs::write(&report, b"ok").unwrap();

        let supervisor = TaskSupervisor::new();
        let id = supervisor.register("bundle_generate", "call-4", None);
        supervisor.mark_running(&id);

        supervisor
            .mark_completed_with_validation(
                &id,
                vec![
                    audio.to_string_lossy().to_string(),
                    image.to_string_lossy().to_string(),
                    video.to_string_lossy().to_string(),
                    report.to_string_lossy().to_string(),
                ],
            )
            .expect("all class-compliant artifacts must pass");

        let task = supervisor.get_task(&id).expect("task missing");
        assert_eq!(task.status, TaskStatus::Completed);
        assert_eq!(task.output_files.len(), 4);
    }

    #[test]
    fn should_treat_empty_file_list_as_completion() {
        let supervisor = TaskSupervisor::new();
        let id = supervisor.register("fm_tts", "call-5", None);
        supervisor.mark_running(&id);

        supervisor
            .mark_completed_with_validation(&id, Vec::new())
            .expect("empty file list should not trip validation");

        let task = supervisor.get_task(&id).expect("task missing");
        assert_eq!(task.status, TaskStatus::Completed);
    }

    #[test]
    fn artifact_mime_class_applies_min_bytes_per_class() {
        assert_eq!(
            ArtifactMimeClass::from_path(Path::new("out.WAV")).min_bytes(),
            1024
        );
        assert_eq!(
            ArtifactMimeClass::from_path(Path::new("out.png")).min_bytes(),
            512
        );
        assert_eq!(
            ArtifactMimeClass::from_path(Path::new("out.mp4")).min_bytes(),
            8192
        );
        assert_eq!(
            ArtifactMimeClass::from_path(Path::new("out.txt")).min_bytes(),
            1
        );
        assert_eq!(
            ArtifactMimeClass::from_path(Path::new("noext")).min_bytes(),
            1
        );
    }

    // ── M8.9: spawn_only failure recovery signals ───────────────────────────

    use std::sync::Mutex as StdMutex;

    fn collect_failure_signals(
        supervisor: &TaskSupervisor,
    ) -> Arc<StdMutex<Vec<SpawnOnlyFailureSignal>>> {
        let collected = Arc::new(StdMutex::new(Vec::new()));
        let captured = Arc::clone(&collected);
        supervisor.set_on_failure_signal(move |signal| {
            captured
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(signal.clone());
        });
        collected
    }

    #[test]
    fn should_emit_failure_signal_when_spawn_only_task_status_becomes_failed() {
        let supervisor = TaskSupervisor::new();
        let collected = collect_failure_signals(&supervisor);
        let task_id = supervisor.register_with_input(
            "fm_tts",
            "call-1",
            Some("api:session"),
            Some(serde_json::json!({"voice": "yangmi", "text": "hi"})),
        );
        supervisor.mark_running(&task_id);
        supervisor.mark_failed(
            &task_id,
            "voice 'yangmi' not registered. available: vivian, serena, longxiang".to_string(),
        );

        let signals = collected.lock().unwrap().clone();
        assert_eq!(signals.len(), 1, "expected exactly one failure signal");
        let signal = &signals[0];
        assert_eq!(signal.task_id, task_id);
        assert_eq!(signal.tool_name, "fm_tts");
        assert_eq!(signal.parent_session_key.as_deref(), Some("api:session"));
        assert!(
            signal
                .error_message
                .contains("voice 'yangmi' not registered")
        );
        assert_eq!(
            signal.suggested_alternatives,
            vec![
                "vivian".to_string(),
                "serena".to_string(),
                "longxiang".to_string()
            ]
        );
        assert_eq!(signal.tool_input["voice"], "yangmi");
    }

    #[test]
    fn should_not_emit_signal_on_successful_completion() {
        let supervisor = TaskSupervisor::new();
        let collected = collect_failure_signals(&supervisor);
        let task_id = supervisor.register("fm_tts", "call-2", None);
        supervisor.mark_running(&task_id);
        supervisor.mark_completed(&task_id, vec!["/tmp/out.mp3".to_string()]);

        assert!(
            collected.lock().unwrap().is_empty(),
            "completion must not emit failure signal"
        );
    }

    #[test]
    fn should_not_emit_signal_on_transient_running_state() {
        let supervisor = TaskSupervisor::new();
        let collected = collect_failure_signals(&supervisor);
        let task_id = supervisor.register("fm_tts", "call-3", None);
        supervisor.mark_running(&task_id);
        supervisor.mark_runtime_state(
            &task_id,
            TaskRuntimeState::DeliveringOutputs,
            Some("send_file".into()),
        );

        assert!(
            collected.lock().unwrap().is_empty(),
            "transient state changes must not emit failure signal"
        );
    }

    #[test]
    fn should_only_emit_failure_signal_once_per_task() {
        let supervisor = TaskSupervisor::new();
        let collected = collect_failure_signals(&supervisor);
        let task_id = supervisor.register("fm_tts", "call-4", None);
        supervisor.mark_running(&task_id);
        supervisor.mark_failed(&task_id, "first failure".to_string());
        // re-marking should NOT re-fire the signal — guards against runaway
        // recovery loops if multiple paths report the same failure.
        supervisor.mark_failed(&task_id, "second failure".to_string());
        supervisor.mark_failed(&task_id, "third failure".to_string());

        assert_eq!(
            collected.lock().unwrap().len(),
            1,
            "subsequent failures must not re-fire the signal"
        );
    }

    #[test]
    fn should_capture_tool_input_in_failure_signal() {
        let supervisor = TaskSupervisor::new();
        let collected = collect_failure_signals(&supervisor);
        let input = serde_json::json!({
            "voice": "yangmi",
            "text": "hello world",
            "format": "mp3",
        });
        let task_id = supervisor.register_with_input("fm_tts", "call-5", None, Some(input.clone()));
        supervisor.mark_failed(&task_id, "internal error".to_string());

        let signals = collected.lock().unwrap().clone();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].tool_input, input);
    }

    #[test]
    fn parse_alternatives_handles_canonical_pattern() {
        let alts = parse_alternatives(
            "voice 'yangmi' not registered. available: vivian, serena, longxiang.",
        );
        assert_eq!(alts, vec!["vivian", "serena", "longxiang"]);
    }

    #[test]
    fn parse_alternatives_returns_empty_when_no_marker() {
        let alts = parse_alternatives("connection refused after 3 retries");
        assert!(alts.is_empty());
    }

    #[test]
    fn parse_alternatives_strips_quotes_and_whitespace() {
        let alts = parse_alternatives(r#"available: "alice", 'bob' , charlie"#);
        assert_eq!(alts, vec!["alice", "bob", "charlie"]);
    }

    #[test]
    fn should_set_tool_input_after_registration() {
        let supervisor = TaskSupervisor::new();
        let collected = collect_failure_signals(&supervisor);
        let task_id = supervisor.register("fm_tts", "call-6", None);
        supervisor.set_tool_input(&task_id, serde_json::json!({"voice": "yangmi"}));
        supervisor.mark_failed(&task_id, "voice missing".to_string());

        let signals = collected.lock().unwrap().clone();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].tool_input["voice"], "yangmi");
    }

    #[test]
    fn should_not_enqueue_second_recovery_for_same_task_id() {
        // Spec-named alias of should_only_emit_failure_signal_once_per_task —
        // codifies that the supervisor-level dedup is what guarantees the
        // session actor never sees a second hint for the same task id.
        let supervisor = TaskSupervisor::new();
        let collected = collect_failure_signals(&supervisor);
        let task_id = supervisor.register("fm_tts", "call-dedup", None);
        supervisor.mark_failed(&task_id, "first".to_string());
        supervisor.mark_failed(&task_id, "second".to_string());
        assert_eq!(collected.lock().unwrap().len(), 1);
    }

    #[test]
    fn should_include_parsed_alternatives_from_error_text() {
        let supervisor = TaskSupervisor::new();
        let collected = collect_failure_signals(&supervisor);
        let task_id = supervisor.register("fm_tts", "call-alts", None);
        supervisor.mark_failed(
            &task_id,
            "voice missing. available: vivian, serena, longxiang.".to_string(),
        );
        let signals = collected.lock().unwrap().clone();
        assert_eq!(signals.len(), 1);
        assert_eq!(
            signals[0].suggested_alternatives,
            vec![
                "vivian".to_string(),
                "serena".to_string(),
                "longxiang".to_string(),
            ]
        );
    }

    #[test]
    fn should_accept_valid_wav_with_real_audio() {
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("voice.wav");
        std::fs::write(&clip, build_sine_wav(1.0, 16_000, false)).unwrap();

        validate_spawn_only_artifacts(&[clip]).expect("real 1s sine WAV must pass all tiers");
    }

    #[test]
    fn should_reject_wav_with_bad_riff_header() {
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("voice.wav");
        // Start with 2 KB so size floor passes but RIFF header is wrong.
        let mut bytes = vec![0u8; 2048];
        bytes[0..4].copy_from_slice(b"RIFX");
        std::fs::write(&clip, &bytes).unwrap();

        let err = validate_spawn_only_artifacts(&[clip])
            .expect_err("WAV with broken RIFF magic must be rejected");
        assert!(
            err.contains("not_a_valid_wav_container"),
            "expected structural rejection, got: {err}"
        );
    }

    #[test]
    fn should_reject_wav_shorter_than_0_2_seconds() {
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("short.wav");
        // 100 ms @ 16 kHz sine. Size floor passes (>=1KB once padded), but
        // duration is below the 0.2 s floor.
        let mut bytes = build_sine_wav(0.1, 16_000, false);
        // The 100ms @ 16kHz 16-bit mono is ~3.2 KB, so size floor passes
        // without padding. Sanity check in case the helper changes:
        if bytes.len() < 1024 {
            bytes.resize(1024, 0);
        }
        std::fs::write(&clip, &bytes).unwrap();

        let err = validate_spawn_only_artifacts(&[clip])
            .expect_err("0.1s WAV must be rejected for duration");
        assert!(
            err.contains("duration_"),
            "expected duration rejection, got: {err}"
        );
    }

    #[test]
    fn should_reject_wav_with_all_silent_samples() {
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("silent.wav");
        std::fs::write(&clip, build_sine_wav(1.0, 16_000, true)).unwrap();

        let err = validate_spawn_only_artifacts(&[clip])
            .expect_err("all-zero PCM must be rejected for silence");
        assert!(
            err.contains("audio_appears_to_be_silence"),
            "expected silence rejection, got: {err}"
        );
    }

    #[test]
    fn should_accept_valid_id3_tagged_mp3() {
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("voice.mp3");
        std::fs::write(&clip, build_id3_tagged_mp3(2048)).unwrap();

        validate_spawn_only_artifacts(&[clip]).expect("ID3v2 magic must pass");
    }

    #[test]
    fn should_accept_valid_mpeg_sync_mp3() {
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("voice.mp3");
        std::fs::write(&clip, build_mpeg_sync_mp3(2048)).unwrap();

        validate_spawn_only_artifacts(&[clip]).expect("0xFF 0xFB MPEG sync must pass");
    }

    #[test]
    fn should_reject_mp3_with_garbage_bytes_no_magic() {
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("voice.mp3");
        // 2 KB of random non-magic bytes. Size passes but magic check fails.
        std::fs::write(&clip, vec![0x42u8; 2048]).unwrap();

        let err = validate_spawn_only_artifacts(&[clip])
            .expect_err("MP3 without ID3 or MPEG sync must be rejected");
        assert!(
            err.contains("mp3_magic_missing"),
            "expected magic rejection, got: {err}"
        );
    }

    #[test]
    fn should_accept_valid_m4a() {
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("voice.m4a");
        std::fs::write(&clip, build_m4a(2048)).unwrap();

        validate_spawn_only_artifacts(&[clip]).expect("ftyp marker at offset 4 must pass");
    }

    #[test]
    fn should_accept_valid_ogg() {
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("voice.ogg");
        std::fs::write(&clip, build_ogg(2048)).unwrap();

        validate_spawn_only_artifacts(&[clip]).expect("OggS magic must pass");
    }

    #[test]
    fn should_accept_valid_flac() {
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("voice.flac");
        std::fs::write(&clip, build_flac(2048)).unwrap();

        validate_spawn_only_artifacts(&[clip]).expect("fLaC magic must pass");
    }

    #[test]
    fn should_include_tool_name_and_input_in_recovery_prompt() {
        // Asserts the supervisor exposes both the tool name and the input
        // on the SpawnOnlyFailureSignal so the session actor can build the
        // recovery prompt without re-walking the message history.
        let supervisor = TaskSupervisor::new();
        let collected = collect_failure_signals(&supervisor);
        let input = serde_json::json!({"voice": "yangmi", "text": "hello"});
        let task_id =
            supervisor.register_with_input("fm_tts", "call-prompt", None, Some(input.clone()));
        supervisor.mark_failed(&task_id, "voice missing".to_string());
        let signals = collected.lock().unwrap().clone();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].tool_name, "fm_tts");
        assert_eq!(signals[0].tool_input, input);
    }

    #[test]
    fn should_emit_failure_signal_with_null_tool_input_when_unset() {
        let supervisor = TaskSupervisor::new();
        let collected = collect_failure_signals(&supervisor);
        let task_id = supervisor.register("fm_tts", "call-7", None);
        supervisor.mark_failed(&task_id, "boom".to_string());

        let signals = collected.lock().unwrap().clone();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].tool_input, Value::Null);
    }

    // ── F004 B2: TaskSupervisor → ToolProgress bridge ─────────────────────

    /// Test reporter that captures every reported event so the bridge
    /// assertions can branch on event kind without parsing JSON.
    struct CapturingReporter {
        events: Arc<StdMutex<Vec<crate::progress::ProgressEvent>>>,
    }

    impl crate::progress::ProgressReporter for CapturingReporter {
        fn report(&self, event: crate::progress::ProgressEvent) {
            self.events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(event);
        }
    }

    fn collect_progress_events(
        supervisor: &TaskSupervisor,
    ) -> Arc<StdMutex<Vec<crate::progress::ProgressEvent>>> {
        let events = Arc::new(StdMutex::new(Vec::new()));
        let reporter = Arc::new(CapturingReporter {
            events: Arc::clone(&events),
        });
        supervisor.set_progress_reporter(reporter);
        events
    }

    fn extract_tool_progress(
        events: &[crate::progress::ProgressEvent],
    ) -> Vec<(String, String, String)> {
        events
            .iter()
            .filter_map(|event| match event {
                crate::progress::ProgressEvent::ToolProgress {
                    name,
                    tool_id,
                    message,
                } => Some((name.clone(), tool_id.clone(), message.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn should_emit_tool_progress_on_runtime_state_transition() {
        let supervisor = TaskSupervisor::new();
        let events = collect_progress_events(&supervisor);
        let task_id = supervisor.register("fm_tts", "call-progress-1", Some("api:session"));
        supervisor.mark_running(&task_id);
        supervisor.mark_runtime_state(
            &task_id,
            TaskRuntimeState::DeliveringOutputs,
            Some("send_file".to_string()),
        );

        let captured = events.lock().unwrap().clone();
        let tool_progress = extract_tool_progress(&captured);
        assert!(
            tool_progress.len() >= 2,
            "expected ToolProgress for mark_running + mark_runtime_state, got: {tool_progress:?}"
        );
        // Last event must reflect the DeliveringOutputs transition and
        // anchor on the originating tool_call_id so the chat UI can route
        // it to the right bubble.
        let (name, tool_id, message) = tool_progress.last().unwrap();
        assert_eq!(name, "fm_tts");
        assert_eq!(tool_id, "call-progress-1");
        assert_eq!(message, "fm_tts: delivering outputs");
    }

    #[test]
    fn should_emit_tool_progress_on_completion_with_tool_call_id() {
        let supervisor = TaskSupervisor::new();
        let events = collect_progress_events(&supervisor);
        let task_id = supervisor.register("podcast_generate", "call-complete-1", None);
        supervisor.mark_completed(&task_id, vec!["/tmp/out.mp3".to_string()]);

        let captured = events.lock().unwrap().clone();
        let tool_progress = extract_tool_progress(&captured);
        let completion = tool_progress
            .iter()
            .find(|(_, _, message)| message.ends_with(": completed"))
            .expect("completion progress event missing");
        assert_eq!(completion.0, "podcast_generate");
        assert_eq!(completion.1, "call-complete-1");
        assert_eq!(completion.2, "podcast_generate: completed");
    }

    #[test]
    fn should_emit_tool_progress_on_failure_with_reason() {
        let supervisor = TaskSupervisor::new();
        let events = collect_progress_events(&supervisor);
        let task_id = supervisor.register("fm_tts", "call-fail-1", None);
        supervisor.mark_failed(&task_id, "workspace policy not found".to_string());

        let captured = events.lock().unwrap().clone();
        let tool_progress = extract_tool_progress(&captured);
        let failure = tool_progress
            .iter()
            .find(|(_, _, message)| message.contains("failed"))
            .expect("failure progress event missing");
        assert_eq!(failure.0, "fm_tts");
        assert_eq!(failure.1, "call-fail-1");
        assert_eq!(failure.2, "fm_tts: failed (workspace policy not found)");
    }

    #[test]
    fn should_not_emit_tool_progress_when_no_reporter_attached() {
        let supervisor = TaskSupervisor::new();
        let task_id = supervisor.register("fm_tts", "call-silent-1", None);
        // No reporter attached — must be a no-op (and crucially must not
        // panic on the missing reporter).
        supervisor.mark_running(&task_id);
        supervisor.mark_runtime_state(
            &task_id,
            TaskRuntimeState::DeliveringOutputs,
            Some("send_file".to_string()),
        );
        supervisor.mark_completed(&task_id, vec![]);
        // Nothing to assert beyond the absence of a panic — the reporter is
        // optional by design so the supervisor can be used outside the
        // chat-progress pipeline (e.g. cron, tests).
    }

    #[test]
    fn should_only_emit_failure_progress_once_per_task() {
        let supervisor = TaskSupervisor::new();
        let events = collect_progress_events(&supervisor);
        let task_id = supervisor.register("fm_tts", "call-fail-dedup", None);
        supervisor.mark_failed(&task_id, "first".to_string());
        // Second mark_failed must NOT re-emit a ToolProgress for the
        // same task — mirrors the existing failure-signal dedup contract.
        supervisor.mark_failed(&task_id, "second".to_string());

        let captured = events.lock().unwrap().clone();
        let tool_progress = extract_tool_progress(&captured);
        let failures: Vec<_> = tool_progress
            .iter()
            .filter(|(_, _, message)| message.contains("failed"))
            .collect();
        assert_eq!(
            failures.len(),
            1,
            "expected exactly one failure ToolProgress, got: {failures:?}"
        );
    }
}
