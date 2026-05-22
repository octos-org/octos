#![allow(dead_code)]
//! M16 ContextManager primitive.
//!
//! This module is intentionally backend-owned and AppUI-agnostic. It provides
//! the canonical transcript, prompt-frame, tool-output envelope, compaction,
//! and fork-sanitizer contracts that SessionActor can wire into the production
//! turn loop in later M16 workstreams.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use chrono::Utc;
use octos_core::{Message, MessageRole, ToolCall};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const CONTEXT_MANAGER_SCHEMA: &str = "octos.context-manager.v1";
const DEFAULT_TOOL_OUTPUT_POLICY_ID: &str = "tool-output-v1";
const TOOL_OUTPUT_UI_PREVIEW_MAX_BYTES: usize = 512;
const SYNTHETIC_MISSING_TOOL_OUTPUT: &str =
    "[tool output missing: aborted before result was recorded]";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) struct TranscriptItemId(String);

impl TranscriptItemId {
    pub(crate) fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ContextCheckpointId(String);

impl ContextCheckpointId {
    pub(crate) fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ContextCompactionId(String);

impl ContextCompactionId {
    pub(crate) fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ContextRecoveryState {
    Exact,
    Rebuilt,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ContextState {
    pub(crate) session_id: String,
    pub(crate) thread_id: Option<String>,
    pub(crate) generation: u64,
    pub(crate) transcript_hash: String,
    pub(crate) last_checkpoint_id: Option<ContextCheckpointId>,
    pub(crate) last_compaction_id: Option<ContextCompactionId>,
    pub(crate) token_estimate: usize,
    pub(crate) item_count: usize,
    pub(crate) recovery_state: ContextRecoveryState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TranscriptItemSource {
    SessionLog,
    AgentLoop,
    ToolRuntime,
    Compaction,
    Supervisor,
    Synthetic,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct TranscriptItem {
    pub(crate) id: TranscriptItemId,
    pub(crate) kind: TranscriptItemKind,
    pub(crate) source: TranscriptItemSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source_ref: Option<TranscriptSourceRef>,
    pub(crate) recorded_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TranscriptSourceRef {
    pub(crate) session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source_seq: Option<usize>,
    pub(crate) source_event_kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ContextSourceRecord {
    pub(crate) item_id: TranscriptItemId,
    pub(crate) session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source_seq: Option<usize>,
    pub(crate) source_event_kind: String,
    pub(crate) transcript_item_kind: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum TranscriptItemKind {
    SystemInstruction {
        content: String,
    },
    DeveloperInstruction {
        content: String,
    },
    UserInput {
        content: String,
        #[serde(default)]
        media: Vec<String>,
    },
    AssistantFinal {
        content: String,
    },
    AssistantReasoning {
        content: String,
    },
    AssistantToolCall {
        call_id: String,
        name: String,
        arguments: Value,
    },
    ToolOutput {
        envelope: ToolOutputEnvelope,
    },
    ContextInjection {
        label: String,
        content: String,
    },
    ChildResultSummary {
        child_agent_id: String,
        summary: String,
        /// Owned artifact refs — the parent has join-level ownership
        /// of these artifacts and may read them through
        /// `task/artifact/read`.
        #[serde(default)]
        artifact_refs: Vec<String>,
        /// #1022 / M17-D — reference-join artifact refs. The parent
        /// can SEE these refs (pointers to the child's artifacts) but
        /// does NOT inherit ownership; the child remains the
        /// authoritative owner. Rendered separately in the parent's
        /// prompt as `References: …` so the model sees pointers, not
        /// copied artifacts. Empty by default; populated only when the
        /// join policy is `reference` rather than `merge`.
        ///
        /// `skip_serializing_if = "Vec::is_empty"` is a backwards-compat
        /// guard (codex P2 follow-up to #1111): legacy snapshots persisted
        /// before this field existed omit the key entirely. Without
        /// `skip_serializing_if`, a freshly-constructed
        /// `ChildResultSummary` with an empty `reference_artifact_refs`
        /// would serialize `"reference_artifact_refs": []` into
        /// `StableTranscriptHashItem`, drifting the transcript hash away
        /// from any pre-#1111 snapshot. Skipping the empty case keeps the
        /// canonical hash identical to the legacy form.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        reference_artifact_refs: Vec<String>,
    },
    CompactionSummary {
        compaction_id: ContextCompactionId,
        summary: String,
        input_transcript_hash: String,
        replacement_transcript_hash: String,
    },
    Checkpoint {
        checkpoint_id: ContextCheckpointId,
        reason: String,
        transcript_hash: String,
    },
    ForkBoundary {
        parent_generation: u64,
        parent_transcript_hash: String,
        policy_id: String,
        sanitizer_hash: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ToolOutputTruncationReason {
    MaxBytes,
    StaleToolResult,
    ContextWindowPressure,
    UnsafeForChildFork,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ToolOutputEnvelope {
    pub(crate) tool_call_id: String,
    pub(crate) tool_name: String,
    pub(crate) raw_sha256: String,
    pub(crate) raw_artifact_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) ui_preview: Option<ToolOutputPreviewLink>,
    pub(crate) original_bytes: usize,
    pub(crate) model_visible_content: String,
    pub(crate) model_visible_bytes: usize,
    pub(crate) truncation_reason: Option<ToolOutputTruncationReason>,
    pub(crate) policy_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ToolOutputPreviewLink {
    pub(crate) preview_ref: String,
    pub(crate) content: String,
    pub(crate) bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ToolOutputPolicy {
    pub(crate) policy_id: String,
    pub(crate) inline_raw_threshold_bytes: usize,
    pub(crate) model_visible_max_bytes: usize,
}

impl Default for ToolOutputPolicy {
    fn default() -> Self {
        Self {
            policy_id: DEFAULT_TOOL_OUTPUT_POLICY_ID.to_owned(),
            inline_raw_threshold_bytes: 16 * 1024,
            model_visible_max_bytes: 4 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PromptBuildPolicy {
    pub(crate) include_reasoning: bool,
    pub(crate) supports_media: bool,
    pub(crate) max_prompt_token_estimate: Option<usize>,
    pub(crate) model_capability_id: String,
}

impl Default for PromptBuildPolicy {
    fn default() -> Self {
        Self {
            include_reasoning: false,
            supports_media: false,
            max_prompt_token_estimate: None,
            model_capability_id: "text-only-v1".to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct NormalizationReport {
    pub(crate) generation: u64,
    pub(crate) input_transcript_hash: String,
    pub(crate) output_prompt_hash: String,
    pub(crate) model_capability_id: String,
    pub(crate) repaired_item_ids: Vec<TranscriptItemId>,
    pub(crate) dropped_item_ids: Vec<TranscriptItemId>,
    pub(crate) synthetic_item_ids: Vec<TranscriptItemId>,
    pub(crate) truncated_item_ids: Vec<TranscriptItemId>,
    pub(crate) token_estimate: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PromptFrame {
    pub(crate) messages: Vec<Message>,
    pub(crate) report: NormalizationReport,
    pub(crate) context_state: ContextState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ForkPolicy {
    pub(crate) policy_id: String,
    pub(crate) keep_last_user_turns: Option<usize>,
}

impl Default for ForkPolicy {
    fn default() -> Self {
        Self {
            policy_id: "child-fork-sanitizer-v1".to_owned(),
            keep_last_user_turns: Some(8),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ContextCompactionStatus {
    Installed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompactContextPolicy {
    pub(crate) policy_id: String,
    pub(crate) trigger: String,
    pub(crate) keep_recent_items: usize,
    pub(crate) preserve_system_instructions: bool,
}

impl Default for CompactContextPolicy {
    fn default() -> Self {
        Self {
            policy_id: "compact-context-v1".to_owned(),
            trigger: "manual".to_owned(),
            keep_recent_items: 8,
            preserve_system_instructions: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ContextCompactionRecord {
    pub(crate) compaction_id: ContextCompactionId,
    pub(crate) status: ContextCompactionStatus,
    pub(crate) policy_id: String,
    pub(crate) trigger: String,
    pub(crate) checkpoint_id: ContextCheckpointId,
    pub(crate) started_at_ms: i64,
    pub(crate) completed_at_ms: i64,
    pub(crate) input_generation: u64,
    pub(crate) output_generation: Option<u64>,
    pub(crate) input_transcript_hash: String,
    pub(crate) replacement_transcript_hash: Option<String>,
    pub(crate) installed_transcript_hash: Option<String>,
    pub(crate) input_item_count: usize,
    pub(crate) retained_item_ids: Vec<TranscriptItemId>,
    pub(crate) dropped_item_ids: Vec<TranscriptItemId>,
    pub(crate) summary_item_id: Option<TranscriptItemId>,
    pub(crate) token_estimate_before: usize,
    pub(crate) token_estimate_after: Option<usize>,
    pub(crate) error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ForkedChildContext {
    pub(crate) parent_generation: u64,
    pub(crate) parent_transcript_hash: String,
    pub(crate) policy_id: String,
    pub(crate) sanitizer_hash: String,
    pub(crate) items: Vec<TranscriptItem>,
    pub(crate) dropped_item_ids: Vec<TranscriptItemId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ContextSnapshot {
    pub(crate) schema: String,
    pub(crate) state: ContextState,
    pub(crate) items: Vec<TranscriptItem>,
    #[serde(default)]
    pub(crate) source_index: Vec<ContextSourceRecord>,
    #[serde(default)]
    pub(crate) compactions: Vec<ContextCompactionRecord>,
}

#[derive(Debug, Clone)]
pub(crate) struct ContextManager {
    session_id: String,
    thread_id: Option<String>,
    generation: u64,
    next_item_seq: u64,
    items: Vec<TranscriptItem>,
    last_checkpoint_id: Option<ContextCheckpointId>,
    last_compaction_id: Option<ContextCompactionId>,
    recovery_state: ContextRecoveryState,
    tool_output_policy: ToolOutputPolicy,
    tool_output_artifacts: HashMap<String, Vec<u8>>,
    compactions: Vec<ContextCompactionRecord>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContextLedgerLoadStatus {
    Loaded,
    Missing,
    Stale,
    Invalid,
}

pub(crate) fn context_ledger_path(data_dir: &Path, session_id: &str) -> PathBuf {
    let encoded = octos_bus::session::encode_path_component(session_id);
    data_dir
        .join("context_ledgers")
        .join(format!("{encoded}.json"))
}

pub(crate) fn persist_context_manager_snapshot(
    data_dir: &Path,
    session_id: &str,
    manager: &ContextManager,
) -> Result<PathBuf, String> {
    persist_tool_output_artifacts(data_dir, manager)?;
    let path = context_ledger_path(data_dir, session_id);
    let parent = path
        .parent()
        .ok_or_else(|| format!("context ledger path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent).map_err(|err| {
        format!(
            "create context ledger directory {} failed: {err}",
            parent.display()
        )
    })?;
    let tmp_name = format!(
        "{}.tmp-{}-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("context-ledger.json"),
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let tmp_path = parent.join(tmp_name);
    let bytes = serde_json::to_vec_pretty(&manager.snapshot())
        .map_err(|err| format!("serialize context ledger snapshot failed: {err}"))?;
    std::fs::write(&tmp_path, bytes).map_err(|err| {
        format!(
            "write context ledger snapshot {} failed: {err}",
            tmp_path.display()
        )
    })?;
    if let Err(err) = std::fs::rename(&tmp_path, &path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(format!(
            "install context ledger snapshot {} failed: {err}",
            path.display()
        ));
    }
    Ok(path)
}

fn context_ledger_artifact_path(data_dir: &Path, artifact_ref: &str) -> Result<PathBuf, String> {
    let root = data_dir.join("context_ledgers");
    let mut path = root.clone();
    for component in Path::new(artifact_ref).components() {
        match component {
            Component::Normal(part) => path.push(part),
            _ => {
                return Err(format!(
                    "invalid context artifact reference contains non-normal component: {artifact_ref}"
                ));
            }
        }
    }
    Ok(path)
}

fn persist_tool_output_artifacts(data_dir: &Path, manager: &ContextManager) -> Result<(), String> {
    for (artifact_ref, bytes) in &manager.tool_output_artifacts {
        let path = context_ledger_artifact_path(data_dir, artifact_ref)?;
        atomic_write_bytes(&path, bytes)?;
    }
    Ok(())
}

fn atomic_write_bytes(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("context artifact path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent).map_err(|err| {
        format!(
            "create context artifact directory {} failed: {err}",
            parent.display()
        )
    })?;
    let tmp_name = format!(
        "{}.tmp-{}-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("tool-output.txt"),
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let tmp_path = parent.join(tmp_name);
    std::fs::write(&tmp_path, bytes).map_err(|err| {
        format!(
            "write context artifact {} failed: {err}",
            tmp_path.display()
        )
    })?;
    if let Err(err) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(format!(
            "install context artifact {} failed: {err}",
            path.display()
        ));
    }
    Ok(())
}

pub(crate) fn load_context_manager_snapshot(
    data_dir: &Path,
    session_id: &str,
) -> Result<Option<ContextManager>, String> {
    let path = context_ledger_path(data_dir, session_id);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path).map_err(|err| {
        format!(
            "read context ledger snapshot {} failed: {err}",
            path.display()
        )
    })?;
    let snapshot = serde_json::from_slice::<ContextSnapshot>(&bytes).map_err(|err| {
        format!(
            "parse context ledger snapshot {} failed: {err}",
            path.display()
        )
    })?;
    if snapshot.state.session_id != session_id {
        return Err(format!(
            "context ledger snapshot {} belongs to session {}, expected {session_id}",
            path.display(),
            snapshot.state.session_id
        ));
    }
    Ok(Some(ContextManager::from_snapshot(snapshot)))
}

pub(crate) fn load_or_rebuild_context_manager(
    data_dir: &Path,
    session_id: impl Into<String>,
    thread_id: Option<String>,
    messages: &[Message],
) -> (ContextManager, ContextLedgerLoadStatus) {
    let session_id = session_id.into();
    match load_context_manager_snapshot(data_dir, &session_id) {
        Ok(Some(manager)) if context_ledger_covers_history(&manager, messages) => {
            (manager, ContextLedgerLoadStatus::Loaded)
        }
        Ok(Some(_)) => {
            let mut rebuilt = ContextManager::from_session_history(session_id, thread_id, messages);
            rebuilt.set_recovery_state(ContextRecoveryState::Rebuilt);
            (rebuilt, ContextLedgerLoadStatus::Stale)
        }
        Ok(None) => {
            let mut rebuilt = ContextManager::from_session_history(session_id, thread_id, messages);
            if !messages.is_empty() {
                rebuilt.set_recovery_state(ContextRecoveryState::Rebuilt);
            }
            (rebuilt, ContextLedgerLoadStatus::Missing)
        }
        Err(_error) => {
            let mut rebuilt = ContextManager::from_session_history(session_id, thread_id, messages);
            rebuilt.set_recovery_state(ContextRecoveryState::Rebuilt);
            (rebuilt, ContextLedgerLoadStatus::Invalid)
        }
    }
}

fn context_ledger_covers_history(manager: &ContextManager, messages: &[Message]) -> bool {
    if messages.is_empty() {
        return manager.items().is_empty();
    }
    let Some(max_source_seq) = manager.source_high_watermark() else {
        return false;
    };
    max_source_seq + 1 >= messages.len()
}

impl ContextManager {
    pub(crate) fn new(session_id: impl Into<String>, thread_id: Option<String>) -> Self {
        Self {
            session_id: session_id.into(),
            thread_id,
            generation: 0,
            next_item_seq: 1,
            items: Vec::new(),
            last_checkpoint_id: None,
            last_compaction_id: None,
            recovery_state: ContextRecoveryState::Exact,
            tool_output_policy: ToolOutputPolicy::default(),
            tool_output_artifacts: HashMap::new(),
            compactions: Vec::new(),
        }
    }

    pub(crate) fn with_tool_output_policy(mut self, policy: ToolOutputPolicy) -> Self {
        self.tool_output_policy = policy;
        self
    }

    pub(crate) fn from_session_history(
        session_id: impl Into<String>,
        thread_id: Option<String>,
        messages: &[Message],
    ) -> Self {
        let mut manager = Self::new(session_id, thread_id);
        for (seq, message) in messages.iter().enumerate() {
            manager.record_persisted_message(message, seq);
        }
        manager
    }

    pub(crate) fn from_forked_child_context(
        session_id: impl Into<String>,
        thread_id: Option<String>,
        fork: ForkedChildContext,
    ) -> Self {
        // Drop any `SystemInstruction` items inherited from a polluted
        // parent context. They are no longer owned by the manager (see
        // `record_message_with_source_ref` early-return) and forking
        // them into the child would re-stack the LLM prompt as soon as
        // the child's `for_prompt` runs.
        let items: Vec<_> = fork
            .items
            .into_iter()
            .filter(|item| !matches!(item.kind, TranscriptItemKind::SystemInstruction { .. }))
            .collect();
        let next_item_seq = items
            .iter()
            .filter_map(|item| {
                item.id
                    .as_str()
                    .strip_prefix("ctxitem_")
                    .and_then(|suffix| suffix.split('_').next())
                    .and_then(|digits| digits.parse::<u64>().ok())
            })
            .max()
            .unwrap_or(0)
            + 1;
        Self {
            session_id: session_id.into(),
            thread_id,
            generation: fork.parent_generation + 1,
            next_item_seq,
            items,
            last_checkpoint_id: None,
            last_compaction_id: None,
            recovery_state: ContextRecoveryState::Rebuilt,
            tool_output_policy: ToolOutputPolicy::default(),
            tool_output_artifacts: HashMap::new(),
            compactions: Vec::new(),
        }
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn items(&self) -> &[TranscriptItem] {
        &self.items
    }

    pub(crate) fn compactions(&self) -> &[ContextCompactionRecord] {
        &self.compactions
    }

    pub(crate) fn set_recovery_state(&mut self, recovery_state: ContextRecoveryState) {
        self.recovery_state = recovery_state;
    }

    pub(crate) fn source_high_watermark(&self) -> Option<usize> {
        self.items
            .iter()
            .filter_map(|item| item.source_ref.as_ref()?.source_seq)
            .max()
    }

    pub(crate) fn state(&self) -> ContextState {
        ContextState {
            session_id: self.session_id.clone(),
            thread_id: self.thread_id.clone(),
            generation: self.generation,
            transcript_hash: self.transcript_hash(),
            last_checkpoint_id: self.last_checkpoint_id.clone(),
            last_compaction_id: self.last_compaction_id.clone(),
            token_estimate: estimate_items_tokens(&self.items),
            item_count: self.items.len(),
            recovery_state: self.recovery_state.clone(),
        }
    }

    pub(crate) fn snapshot(&self) -> ContextSnapshot {
        ContextSnapshot {
            schema: CONTEXT_MANAGER_SCHEMA.to_owned(),
            state: self.state(),
            items: self.items.clone(),
            source_index: self.source_index(),
            compactions: self.compactions.clone(),
        }
    }

    pub(crate) fn source_index(&self) -> Vec<ContextSourceRecord> {
        self.items
            .iter()
            .filter_map(|item| {
                let source_ref = item.source_ref.as_ref()?;
                Some(ContextSourceRecord {
                    item_id: item.id.clone(),
                    session_id: source_ref.session_id.clone(),
                    thread_id: source_ref.thread_id.clone(),
                    source_seq: source_ref.source_seq,
                    source_event_kind: source_ref.source_event_kind.clone(),
                    transcript_item_kind: transcript_item_kind_name(&item.kind).to_owned(),
                })
            })
            .collect()
    }

    pub(crate) fn from_snapshot(snapshot: ContextSnapshot) -> Self {
        // Strip `SystemInstruction` items inherited from a polluted
        // snapshot. Pre-fix daemons (between 28552bb9d landing and the
        // System-skip fix on `record_message_with_source_ref`) stacked
        // one SystemInstruction per turn into the manager and
        // persisted it; reloading those snapshots without filtering
        // would resurrect the duplicates on the next `for_prompt`.
        // `SystemInstruction` items are no longer owned by the manager,
        // so dropping them here is the snapshot-side complement to the
        // recording-side early-return.
        let items: Vec<_> = snapshot
            .items
            .into_iter()
            .filter(|item| !matches!(item.kind, TranscriptItemKind::SystemInstruction { .. }))
            .collect();
        let next_item_seq = items
            .iter()
            .filter_map(|item| item.id.as_str().strip_prefix("ctxitem_"))
            .filter_map(|suffix| suffix.parse::<u64>().ok())
            .max()
            .unwrap_or(0)
            + 1;
        Self {
            session_id: snapshot.state.session_id,
            thread_id: snapshot.state.thread_id,
            generation: snapshot.state.generation,
            next_item_seq,
            items,
            last_checkpoint_id: snapshot.state.last_checkpoint_id,
            last_compaction_id: snapshot.state.last_compaction_id,
            recovery_state: snapshot.state.recovery_state,
            tool_output_policy: ToolOutputPolicy::default(),
            tool_output_artifacts: HashMap::new(),
            compactions: snapshot.compactions,
        }
    }

    pub(crate) fn transcript_hash(&self) -> String {
        hash_json(&json!({
            "schema": CONTEXT_MANAGER_SCHEMA,
            "session_id": self.session_id,
            "thread_id": self.thread_id,
            "generation": self.generation,
            "items": self.items.iter().map(StableTranscriptHashItem::from).collect::<Vec<_>>(),
        }))
    }

    pub(crate) fn record_item(
        &mut self,
        kind: TranscriptItemKind,
        source: TranscriptItemSource,
    ) -> TranscriptItemId {
        self.record_item_with_source_ref(kind, source, None)
    }

    pub(crate) fn record_item_with_source_ref(
        &mut self,
        kind: TranscriptItemKind,
        source: TranscriptItemSource,
        source_ref: Option<TranscriptSourceRef>,
    ) -> TranscriptItemId {
        let id = self.next_item_id();
        self.items.push(TranscriptItem {
            id: id.clone(),
            kind,
            source,
            source_ref,
            recorded_at_ms: Utc::now().timestamp_millis(),
        });
        self.generation += 1;
        id
    }

    pub(crate) fn record_message(&mut self, message: &Message) -> Vec<TranscriptItemId> {
        self.record_message_with_source_ref(message, None)
    }

    pub(crate) fn record_persisted_message(
        &mut self,
        message: &Message,
        source_seq: usize,
    ) -> Vec<TranscriptItemId> {
        self.record_message_with_source_ref(
            message,
            Some(TranscriptSourceRef {
                session_id: self.session_id.clone(),
                thread_id: message.thread_id.clone().or_else(|| self.thread_id.clone()),
                source_seq: Some(source_seq),
                source_event_kind: message.role.as_str().to_owned(),
            }),
        )
    }

    pub(crate) fn record_persisted_message_merging_prompt_equivalent(
        &mut self,
        message: &Message,
        source_seq: usize,
    ) -> Vec<TranscriptItemId> {
        let source_ref = TranscriptSourceRef {
            session_id: self.session_id.clone(),
            thread_id: message.thread_id.clone().or_else(|| self.thread_id.clone()),
            source_seq: Some(source_seq),
            source_event_kind: message.role.as_str().to_owned(),
        };
        let mut probe = ContextManager::new(self.session_id.clone(), self.thread_id.clone())
            .with_tool_output_policy(self.tool_output_policy.clone());
        probe.record_message(message);

        // #982: the probe was built from a fresh, empty transcript so its
        // Tool branch resolved to `tool_name: "unknown"`. Rewrite any
        // freshly-built `ToolOutput` envelope with the real tool_name
        // we can look up against `self.items`.
        for probe_item in probe.items.iter_mut() {
            if let TranscriptItemKind::ToolOutput { envelope } = &mut probe_item.kind {
                if envelope.tool_name == "unknown" {
                    if let Some(real_name) = self.tool_name_for_call_id(&envelope.tool_call_id) {
                        envelope.tool_name = real_name;
                    }
                }
            }
        }

        let mut ids = Vec::new();
        for probe_item in probe.items {
            if let Some(existing) = self
                .items
                .iter_mut()
                .find(|item| item.source_ref.is_none() && item.kind == probe_item.kind)
            {
                existing.source_ref = Some(source_ref.clone());
                self.generation += 1;
                ids.push(existing.id.clone());
            } else {
                ids.push(self.record_item_with_source_ref(
                    probe_item.kind,
                    probe_item.source,
                    Some(source_ref.clone()),
                ));
            }
        }
        ids
    }

    pub(crate) fn record_message_with_source_ref(
        &mut self,
        message: &Message,
        source_ref: Option<TranscriptSourceRef>,
    ) -> Vec<TranscriptItemId> {
        // The agent's runtime System prompt is composed per turn via
        // `compose_system_prompt()` and placed at `messages[0]` by the
        // agent loop; it is NOT a piece of session state the
        // ContextManager should own. Recording it here makes every
        // entry point (bridge per-turn recording, persisted-message
        // merge, boot replay via from_session_history, fork) stack a
        // `SystemInstruction` item across turns / restarts. `for_prompt`
        // then re-emits them all and `normalize_system_messages`
        // concatenates them into a 4×-bloated `messages[0]`.
        //
        // The legitimate use of SystemInstruction items (compaction
        // summaries) flows through `compact_context`, not through this
        // recording API. Skipping System here is therefore safe across
        // all callers: bridge recording, persisted-message merging,
        // boot replay, and snapshot reconstruction.
        if message.role == MessageRole::System {
            return Vec::new();
        }
        let mut ids = Vec::new();
        match message.role {
            MessageRole::System => ids.push(self.record_item_with_source_ref(
                TranscriptItemKind::SystemInstruction {
                    content: message.content.clone(),
                },
                TranscriptItemSource::SessionLog,
                source_ref.clone(),
            )),
            MessageRole::User => ids.push(self.record_item_with_source_ref(
                TranscriptItemKind::UserInput {
                    content: message.content.clone(),
                    media: message.media.clone(),
                },
                TranscriptItemSource::SessionLog,
                source_ref.clone(),
            )),
            MessageRole::Assistant => {
                if let Some(reasoning) = message.reasoning_content.as_ref() {
                    ids.push(self.record_item_with_source_ref(
                        TranscriptItemKind::AssistantReasoning {
                            content: reasoning.clone(),
                        },
                        TranscriptItemSource::AgentLoop,
                        source_ref.clone(),
                    ));
                }
                for tool_call in message.tool_calls.iter().flatten() {
                    ids.push(self.record_item_with_source_ref(
                        TranscriptItemKind::AssistantToolCall {
                            call_id: tool_call.id.clone(),
                            name: tool_call.name.clone(),
                            arguments: tool_call.arguments.clone(),
                        },
                        TranscriptItemSource::AgentLoop,
                        source_ref.clone(),
                    ));
                }
                if !message.content.trim().is_empty() {
                    ids.push(self.record_item_with_source_ref(
                        TranscriptItemKind::AssistantFinal {
                            content: message.content.clone(),
                        },
                        TranscriptItemSource::AgentLoop,
                        source_ref.clone(),
                    ));
                }
            }
            MessageRole::Tool => {
                if let Some(tool_call_id) = message.tool_call_id.as_ref() {
                    // #982: resolve the real tool_name from a prior
                    // AssistantToolCall transcript entry instead of
                    // burning the placeholder "unknown" into the
                    // ToolOutputEnvelope. Replay over the persisted
                    // transcript must reconstruct identical model-visible
                    // tool output, which means the envelope must carry
                    // the same tool_name the LLM saw.
                    let tool_name = self
                        .tool_name_for_call_id(tool_call_id)
                        .unwrap_or_else(|| "unknown".to_owned());
                    ids.push(self.record_tool_output_with_source_ref(
                        tool_call_id,
                        tool_name,
                        &message.content,
                        source_ref.clone(),
                    ));
                }
            }
        }
        ids
    }

    /// Walk `self.items` from newest to oldest looking for an
    /// `AssistantToolCall` matching `tool_call_id`, returning its
    /// `name` for envelope wiring. #982: lets `MessageRole::Tool`
    /// recording carry the real tool name into `ToolOutputEnvelope`.
    fn tool_name_for_call_id(&self, tool_call_id: &str) -> Option<String> {
        self.items.iter().rev().find_map(|item| match &item.kind {
            TranscriptItemKind::AssistantToolCall { call_id, name, .. }
                if call_id == tool_call_id =>
            {
                Some(name.clone())
            }
            _ => None,
        })
    }

    pub(crate) fn record_tool_output(
        &mut self,
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        raw_output: &str,
    ) -> TranscriptItemId {
        self.record_tool_output_with_source_ref(tool_call_id, tool_name, raw_output, None)
    }

    pub(crate) fn record_tool_output_with_source_ref(
        &mut self,
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        raw_output: &str,
        source_ref: Option<TranscriptSourceRef>,
    ) -> TranscriptItemId {
        let tool_call_id = tool_call_id.into();
        let raw_sha256 = sha256_prefixed(raw_output.as_bytes());
        let original_bytes = raw_output.len();
        let (model_visible_content, truncation_reason) = truncate_utf8(
            raw_output,
            self.tool_output_policy.model_visible_max_bytes,
            ToolOutputTruncationReason::MaxBytes,
        );
        let raw_artifact_ref = (truncation_reason.is_some()
            || original_bytes > self.tool_output_policy.inline_raw_threshold_bytes)
            .then(|| format!("tool-output/{raw_sha256}.txt"));
        if let Some(artifact_ref) = raw_artifact_ref.as_ref() {
            self.tool_output_artifacts
                .insert(artifact_ref.clone(), raw_output.as_bytes().to_vec());
        }
        let ui_preview_content = tool_output_preview(&model_visible_content);
        let ui_preview = Some(ToolOutputPreviewLink {
            preview_ref: format!("appui/tool-output-preview/{tool_call_id}"),
            bytes: ui_preview_content.len(),
            content: ui_preview_content,
        });
        self.record_item_with_source_ref(
            TranscriptItemKind::ToolOutput {
                envelope: ToolOutputEnvelope {
                    tool_call_id,
                    tool_name: tool_name.into(),
                    raw_sha256,
                    raw_artifact_ref,
                    ui_preview,
                    original_bytes,
                    model_visible_bytes: model_visible_content.len(),
                    model_visible_content,
                    truncation_reason,
                    policy_id: self.tool_output_policy.policy_id.clone(),
                },
            },
            TranscriptItemSource::ToolRuntime,
            source_ref,
        )
    }

    /// #1022 / M17-D — production writer for a bounded
    /// `ChildResultSummary` capsule.
    ///
    /// Called by the parent session's join site when a child task
    /// terminates: it folds a model-generated `summary` plus the
    /// artifact-ref pointers (`artifact_refs` for owned, joined-into
    /// artifacts; `reference_artifact_refs` for pointer-only references
    /// the parent can see but does not own) into the parent's transcript
    /// as a single supervisor-sourced item. The capsule is then rendered
    /// by `for_prompt` as a bounded assistant message with the form
    /// `[child <id> summary]\n<summary>\nArtifacts: …\nReferences: …`,
    /// keeping the child's raw transcript out of the parent's prompt
    /// (see SubagentResultCapsule contract).
    ///
    /// The `summary` argument is the caller's responsibility to bound —
    /// the writer does not re-summarize or truncate, by design: the
    /// model that produced the summary already knows the bound. The
    /// writer's job is only to install the capsule into the parent's
    /// transcript with the correct `Supervisor` source attribution.
    pub(crate) fn record_child_result_summary(
        &mut self,
        child_agent_id: impl Into<String>,
        summary: impl Into<String>,
        artifact_refs: Vec<String>,
        reference_artifact_refs: Vec<String>,
    ) -> TranscriptItemId {
        self.record_item(
            TranscriptItemKind::ChildResultSummary {
                child_agent_id: child_agent_id.into(),
                summary: summary.into(),
                artifact_refs,
                reference_artifact_refs,
            },
            TranscriptItemSource::Supervisor,
        )
    }

    pub(crate) fn checkpoint(&mut self, reason: impl Into<String>) -> ContextCheckpointId {
        let checkpoint_id = ContextCheckpointId::new(format!("ctxchk_{:06}", self.generation + 1));
        let transcript_hash = self.transcript_hash();
        self.record_item(
            TranscriptItemKind::Checkpoint {
                checkpoint_id: checkpoint_id.clone(),
                reason: reason.into(),
                transcript_hash,
            },
            TranscriptItemSource::Synthetic,
        );
        self.last_checkpoint_id = Some(checkpoint_id.clone());
        checkpoint_id
    }

    pub(crate) fn install_compaction_summary(
        &mut self,
        summary: impl Into<String>,
        keep_recent_items: usize,
    ) -> ContextCompactionId {
        let record = self.compact_context(
            summary,
            CompactContextPolicy {
                keep_recent_items,
                ..CompactContextPolicy::default()
            },
        );
        record.compaction_id
    }

    pub(crate) fn compact_context(
        &mut self,
        summary: impl Into<String>,
        policy: CompactContextPolicy,
    ) -> ContextCompactionRecord {
        let summary = summary.into();
        let started_at_ms = Utc::now().timestamp_millis();
        let input_generation = self.generation;
        let input_hash = self.transcript_hash();
        let input_item_count = self.items.len();
        let token_estimate_before = estimate_items_tokens(&self.items);
        let compaction_id = self.next_compaction_id();
        let checkpoint_id = self.next_checkpoint_id();
        let policy_id = policy.policy_id.clone();
        let trigger = policy.trigger.clone();
        let (mut retained, retained_item_ids, dropped_item_ids) =
            self.compaction_replacement_items(&policy);
        let replacement_transcript_hash = hash_json(&json!({
            "schema": CONTEXT_MANAGER_SCHEMA,
            "compaction_id": compaction_id,
            "policy_id": policy_id,
            "trigger": trigger,
            "summary": summary,
            "retained_item_ids": retained_item_ids.iter().map(TranscriptItemId::as_str).collect::<Vec<_>>(),
        }));
        let summary_item_id = self.next_item_id();
        let compaction_item = TranscriptItem {
            id: summary_item_id.clone(),
            kind: TranscriptItemKind::CompactionSummary {
                compaction_id: compaction_id.clone(),
                summary,
                input_transcript_hash: input_hash.clone(),
                replacement_transcript_hash: replacement_transcript_hash.clone(),
            },
            source: TranscriptItemSource::Compaction,
            source_ref: None,
            recorded_at_ms: Utc::now().timestamp_millis(),
        };
        retained.insert(compaction_summary_insert_index(&retained), compaction_item);
        self.items = retained;
        self.generation = input_generation + 1;
        self.last_checkpoint_id = Some(checkpoint_id.clone());
        self.last_compaction_id = Some(compaction_id.clone());
        let installed_transcript_hash = self.transcript_hash();
        let record = ContextCompactionRecord {
            compaction_id,
            status: ContextCompactionStatus::Installed,
            policy_id,
            trigger,
            checkpoint_id,
            started_at_ms,
            completed_at_ms: Utc::now().timestamp_millis(),
            input_generation,
            output_generation: Some(self.generation),
            input_transcript_hash: input_hash,
            replacement_transcript_hash: Some(replacement_transcript_hash),
            installed_transcript_hash: Some(installed_transcript_hash),
            input_item_count,
            retained_item_ids,
            dropped_item_ids,
            summary_item_id: Some(summary_item_id),
            token_estimate_before,
            token_estimate_after: Some(estimate_items_tokens(&self.items)),
            error: None,
        };
        self.compactions.push(record.clone());
        record
    }

    pub(crate) fn record_failed_compaction(
        &mut self,
        policy: CompactContextPolicy,
        error: impl Into<String>,
    ) -> ContextCompactionRecord {
        let now = Utc::now().timestamp_millis();
        let input_hash = self.transcript_hash();
        let record = ContextCompactionRecord {
            compaction_id: self.next_compaction_id(),
            status: ContextCompactionStatus::Failed,
            policy_id: policy.policy_id,
            trigger: policy.trigger,
            checkpoint_id: self.next_checkpoint_id(),
            started_at_ms: now,
            completed_at_ms: now,
            input_generation: self.generation,
            output_generation: None,
            input_transcript_hash: input_hash,
            replacement_transcript_hash: None,
            installed_transcript_hash: None,
            input_item_count: self.items.len(),
            retained_item_ids: Vec::new(),
            dropped_item_ids: Vec::new(),
            summary_item_id: None,
            token_estimate_before: estimate_items_tokens(&self.items),
            token_estimate_after: None,
            error: Some(error.into()),
        };
        self.compactions.push(record.clone());
        record
    }

    pub(crate) fn for_prompt(&self, policy: &PromptBuildPolicy) -> PromptFrame {
        let mut entries = Vec::new();
        let mut dropped_item_ids = Vec::new();
        let mut repaired_item_ids = Vec::new();
        let mut synthetic_item_ids = Vec::new();
        let mut truncated_item_ids = Vec::new();
        let mut emitted_tool_calls = HashSet::new();
        let mut emitted_tool_outputs = HashSet::new();
        let mut index = 0;

        while index < self.items.len() {
            let item = &self.items[index];
            match &item.kind {
                TranscriptItemKind::SystemInstruction { content }
                | TranscriptItemKind::DeveloperInstruction { content } => {
                    entries.push(PromptMessageEntry::protected(
                        message(MessageRole::System, content.clone()),
                        item.id.clone(),
                    ));
                    index += 1;
                }
                TranscriptItemKind::UserInput { content, media } => {
                    let mut msg = message(MessageRole::User, content.clone());
                    if policy.supports_media {
                        msg.media = media.clone();
                    } else if !media.is_empty() {
                        repaired_item_ids.push(item.id.clone());
                    }
                    entries.push(PromptMessageEntry::new(msg, item.id.clone()));
                    index += 1;
                }
                TranscriptItemKind::AssistantFinal { content } => {
                    entries.push(PromptMessageEntry::new(
                        message(MessageRole::Assistant, content.clone()),
                        item.id.clone(),
                    ));
                    index += 1;
                }
                TranscriptItemKind::AssistantReasoning { content } => {
                    if policy.include_reasoning {
                        let mut msg = message(MessageRole::Assistant, String::new());
                        msg.reasoning_content = Some(content.clone());
                        entries.push(PromptMessageEntry::new(msg, item.id.clone()));
                    } else {
                        dropped_item_ids.push(item.id.clone());
                    }
                    index += 1;
                }
                TranscriptItemKind::AssistantToolCall { .. } => {
                    let mut source_item_ids = Vec::new();
                    let mut call_ids = Vec::new();
                    let mut calls = Vec::new();

                    while let Some(item) = self.items.get(index) {
                        let TranscriptItemKind::AssistantToolCall {
                            call_id,
                            name,
                            arguments,
                        } = &item.kind
                        else {
                            break;
                        };
                        emitted_tool_calls.insert(call_id.clone());
                        source_item_ids.push(item.id.clone());
                        call_ids.push(call_id.clone());
                        calls.push(ToolCall {
                            id: call_id.clone(),
                            name: name.clone(),
                            arguments: arguments.clone(),
                            metadata: None,
                        });
                        index += 1;
                    }

                    let mut content = String::new();
                    if let Some(next_item) = self.items.get(index) {
                        if let TranscriptItemKind::AssistantFinal {
                            content: next_content,
                        } = &next_item.kind
                        {
                            content = next_content.clone();
                            source_item_ids.push(next_item.id.clone());
                            index += 1;
                        }
                    }

                    let mut msg = message(MessageRole::Assistant, content);
                    msg.tool_calls = Some(calls);
                    entries.push(PromptMessageEntry::tool_call_group(
                        msg,
                        source_item_ids,
                        call_ids.clone(),
                    ));

                    for call_id in call_ids {
                        if let Some((tool_item_id, envelope)) =
                            self.items.iter().find_map(|candidate| {
                                let TranscriptItemKind::ToolOutput { envelope } = &candidate.kind
                                else {
                                    return None;
                                };
                                (envelope.tool_call_id == call_id)
                                    .then_some((candidate.id.clone(), envelope))
                            })
                        {
                            emitted_tool_outputs.insert(call_id.clone());
                            let mut msg =
                                message(MessageRole::Tool, envelope.model_visible_content.clone());
                            msg.tool_call_id = Some(call_id.clone());
                            entries.push(PromptMessageEntry::tool_output(
                                msg,
                                tool_item_id.clone(),
                                call_id.clone(),
                            ));
                            if envelope.truncation_reason.is_some() {
                                truncated_item_ids.push(tool_item_id);
                            }
                        } else {
                            let synthetic_id =
                                TranscriptItemId::new(format!("synthetic_tool_output_{call_id}"));
                            synthetic_item_ids.push(synthetic_id.clone());
                            let mut synthetic =
                                message(MessageRole::Tool, SYNTHETIC_MISSING_TOOL_OUTPUT);
                            synthetic.tool_call_id = Some(call_id.clone());
                            entries.push(PromptMessageEntry::tool_output(
                                synthetic,
                                synthetic_id,
                                call_id.clone(),
                            ));
                        }
                    }
                }
                TranscriptItemKind::ToolOutput { envelope } => {
                    if emitted_tool_outputs.contains(&envelope.tool_call_id) {
                        index += 1;
                        continue;
                    }
                    if emitted_tool_calls.contains(&envelope.tool_call_id) {
                        let mut msg =
                            message(MessageRole::Tool, envelope.model_visible_content.clone());
                        msg.tool_call_id = Some(envelope.tool_call_id.clone());
                        entries.push(PromptMessageEntry::tool_output(
                            msg,
                            item.id.clone(),
                            envelope.tool_call_id.clone(),
                        ));
                        if envelope.truncation_reason.is_some() {
                            truncated_item_ids.push(item.id.clone());
                        }
                    } else {
                        dropped_item_ids.push(item.id.clone());
                    }
                    index += 1;
                }
                TranscriptItemKind::ContextInjection { label, content } => {
                    entries.push(PromptMessageEntry::protected(
                        message(
                            MessageRole::System,
                            format!("[context injection: {label}]\n{content}"),
                        ),
                        item.id.clone(),
                    ));
                    index += 1;
                }
                TranscriptItemKind::ChildResultSummary {
                    child_agent_id,
                    summary,
                    artifact_refs,
                    reference_artifact_refs,
                } => {
                    let artifacts = if artifact_refs.is_empty() {
                        String::new()
                    } else {
                        format!("\nArtifacts: {}", artifact_refs.join(", "))
                    };
                    // #1022 / M17-D — reference-join refs render as a
                    // separate "References:" line so the model can tell
                    // pointer-to-child from owned-by-join artifacts.
                    let references = if reference_artifact_refs.is_empty() {
                        String::new()
                    } else {
                        format!("\nReferences: {}", reference_artifact_refs.join(", "))
                    };
                    let summary_text = format!(
                        "[child {child_agent_id} summary]\n{summary}{artifacts}{references}"
                    );
                    entries.push(PromptMessageEntry::new(
                        message(MessageRole::Assistant, summary_text),
                        item.id.clone(),
                    ));
                    index += 1;
                }
                TranscriptItemKind::CompactionSummary { summary, .. } => {
                    entries.push(PromptMessageEntry::protected(
                        message(
                            MessageRole::System,
                            format!("[Conversation summary]\n{summary}"),
                        ),
                        item.id.clone(),
                    ));
                    index += 1;
                }
                TranscriptItemKind::Checkpoint { .. } | TranscriptItemKind::ForkBoundary { .. } => {
                    dropped_item_ids.push(item.id.clone());
                    index += 1;
                }
            }
        }

        if let Some(max_tokens) = policy.max_prompt_token_estimate {
            truncate_tool_outputs_for_context_pressure(
                &mut entries,
                max_tokens,
                &mut truncated_item_ids,
            );
            trim_prompt_entries_preserving_invariants(
                &mut entries,
                max_tokens,
                &mut dropped_item_ids,
            );
        }
        let messages = entries
            .into_iter()
            .map(|entry| entry.message)
            .collect::<Vec<_>>();
        let token_estimate = estimate_messages_tokens(&messages);
        let output_prompt_hash = hash_prompt_messages(&messages);
        let report = NormalizationReport {
            generation: self.generation,
            input_transcript_hash: self.transcript_hash(),
            output_prompt_hash,
            model_capability_id: policy.model_capability_id.clone(),
            repaired_item_ids,
            dropped_item_ids,
            synthetic_item_ids,
            truncated_item_ids,
            token_estimate,
        };
        PromptFrame {
            messages,
            report,
            context_state: self.state(),
        }
    }

    pub(crate) fn fork_child_history(&self, policy: &ForkPolicy) -> ForkedChildContext {
        let cutoff = fork_cutoff_index(&self.items, policy.keep_last_user_turns);
        let parent_hash = self.transcript_hash();
        let mut kept = Vec::new();
        let mut dropped = Vec::new();
        for (index, item) in self.items.iter().enumerate() {
            if should_keep_for_child(item, index, cutoff) {
                kept.push(item.clone());
            } else {
                dropped.push(item.id.clone());
            }
        }
        let sanitizer_hash = hash_json(&json!({
            "policy": policy,
            "parent_generation": self.generation,
            "parent_transcript_hash": parent_hash,
            "kept_item_ids": kept.iter().map(|item| item.id.as_str()).collect::<Vec<_>>(),
        }));
        kept.push(TranscriptItem {
            id: TranscriptItemId::new(format!("ctxitem_fork_{:06}", self.generation + 1)),
            kind: TranscriptItemKind::ForkBoundary {
                parent_generation: self.generation,
                parent_transcript_hash: parent_hash.clone(),
                policy_id: policy.policy_id.clone(),
                sanitizer_hash: sanitizer_hash.clone(),
            },
            source: TranscriptItemSource::Synthetic,
            source_ref: None,
            recorded_at_ms: Utc::now().timestamp_millis(),
        });
        ForkedChildContext {
            parent_generation: self.generation,
            parent_transcript_hash: parent_hash,
            policy_id: policy.policy_id.clone(),
            sanitizer_hash,
            items: kept,
            dropped_item_ids: dropped,
        }
    }

    fn next_item_id(&mut self) -> TranscriptItemId {
        let id = TranscriptItemId::new(format!("ctxitem_{:06}", self.next_item_seq));
        self.next_item_seq += 1;
        id
    }

    fn next_checkpoint_id(&self) -> ContextCheckpointId {
        ContextCheckpointId::new(format!(
            "ctxchk_{:06}_{}",
            self.generation + 1,
            Utc::now().timestamp_millis()
        ))
    }

    fn next_compaction_id(&self) -> ContextCompactionId {
        ContextCompactionId::new(format!(
            "ctxcmp_{:06}_{}",
            self.generation + 1,
            Utc::now().timestamp_millis()
        ))
    }

    fn compaction_replacement_items(
        &self,
        policy: &CompactContextPolicy,
    ) -> (
        Vec<TranscriptItem>,
        Vec<TranscriptItemId>,
        Vec<TranscriptItemId>,
    ) {
        let mut retained = Vec::new();
        let mut retained_ids = HashSet::new();
        if policy.preserve_system_instructions {
            for item in self
                .items
                .iter()
                .filter(|item| matches!(item.kind, TranscriptItemKind::SystemInstruction { .. }))
            {
                retained_ids.insert(item.id.clone());
                retained.push(item.clone());
            }
        }

        let recent_start = self.items.len().saturating_sub(policy.keep_recent_items);
        for item in self.items.iter().skip(recent_start) {
            if retained_ids.insert(item.id.clone()) {
                retained.push(item.clone());
            }
        }

        let retained_item_ids = retained
            .iter()
            .map(|item| item.id.clone())
            .collect::<Vec<_>>();
        let dropped_item_ids = self
            .items
            .iter()
            .filter(|item| !retained_ids.contains(&item.id))
            .map(|item| item.id.clone())
            .collect::<Vec<_>>();
        (retained, retained_item_ids, dropped_item_ids)
    }
}

#[derive(Debug, Serialize)]
struct StableTranscriptHashItem<'a> {
    id: &'a str,
    kind: &'a TranscriptItemKind,
}

impl<'a> From<&'a TranscriptItem> for StableTranscriptHashItem<'a> {
    fn from(item: &'a TranscriptItem) -> Self {
        Self {
            id: item.id.as_str(),
            kind: &item.kind,
        }
    }
}

#[derive(Debug)]
struct PromptMessageEntry {
    message: Message,
    source_item_ids: Vec<TranscriptItemId>,
    protected: bool,
    tool_call_ids: Vec<String>,
    tool_output_call_id: Option<String>,
}

impl PromptMessageEntry {
    fn new(message: Message, source_item_id: TranscriptItemId) -> Self {
        Self {
            message,
            source_item_ids: vec![source_item_id],
            protected: false,
            tool_call_ids: Vec::new(),
            tool_output_call_id: None,
        }
    }

    fn protected(message: Message, source_item_id: TranscriptItemId) -> Self {
        Self {
            protected: true,
            ..Self::new(message, source_item_id)
        }
    }

    fn tool_call(message: Message, source_item_id: TranscriptItemId, call_id: String) -> Self {
        Self {
            tool_call_ids: vec![call_id],
            ..Self::new(message, source_item_id)
        }
    }

    fn tool_call_group(
        message: Message,
        source_item_ids: Vec<TranscriptItemId>,
        call_ids: Vec<String>,
    ) -> Self {
        Self {
            message,
            source_item_ids,
            protected: false,
            tool_call_ids: call_ids,
            tool_output_call_id: None,
        }
    }

    fn tool_output(message: Message, source_item_id: TranscriptItemId, call_id: String) -> Self {
        Self {
            tool_output_call_id: Some(call_id),
            ..Self::new(message, source_item_id)
        }
    }
}

fn trim_prompt_entries_preserving_invariants(
    entries: &mut Vec<PromptMessageEntry>,
    max_tokens: usize,
    dropped_item_ids: &mut Vec<TranscriptItemId>,
) {
    while estimate_entries_tokens(entries) > max_tokens && entries.len() > 1 {
        let Some((start, end)) = first_removable_prompt_group(entries) else {
            break;
        };
        let removed = entries.drain(start..end).collect::<Vec<_>>();
        for entry in removed {
            for item_id in entry.source_item_ids {
                push_unique_item_id(dropped_item_ids, item_id);
            }
        }
    }
}

fn truncate_tool_outputs_for_context_pressure(
    entries: &mut [PromptMessageEntry],
    max_tokens: usize,
    truncated_item_ids: &mut Vec<TranscriptItemId>,
) {
    if estimate_entries_tokens(entries) <= max_tokens {
        return;
    }
    let max_tool_bytes = (max_tokens.saturating_mul(4) / 2).max(1);
    for entry in entries.iter_mut() {
        if entry.message.role != MessageRole::Tool || entry.message.content.len() <= max_tool_bytes
        {
            continue;
        }
        let (content, reason) = truncate_utf8(
            &entry.message.content,
            max_tool_bytes,
            ToolOutputTruncationReason::ContextWindowPressure,
        );
        if reason.is_some() {
            entry.message.content = content;
            for item_id in &entry.source_item_ids {
                push_unique_item_id(truncated_item_ids, item_id.clone());
            }
        }
    }
}

fn first_removable_prompt_group(entries: &[PromptMessageEntry]) -> Option<(usize, usize)> {
    let start = entries
        .iter()
        .position(|entry| !entry.protected && !matches!(entry.message.role, MessageRole::Tool))?;
    let mut end = start + 1;

    if matches!(entries[start].message.role, MessageRole::User) {
        while end < entries.len()
            && !entries[end].protected
            && !matches!(entries[end].message.role, MessageRole::User)
        {
            end = extend_tool_call_group(entries, end);
        }
        return Some((start, end));
    }

    end = extend_matching_tool_outputs(entries, end, &entries[start].tool_call_ids);
    Some((start, end))
}

fn extend_tool_call_group(entries: &[PromptMessageEntry], start: usize) -> usize {
    let end = start + 1;
    extend_matching_tool_outputs(entries, end, &entries[start].tool_call_ids)
}

fn extend_matching_tool_outputs(
    entries: &[PromptMessageEntry],
    mut end: usize,
    call_ids: &[String],
) -> usize {
    while end < entries.len()
        && !entries[end].protected
        && entries[end]
            .tool_output_call_id
            .as_ref()
            .is_some_and(|call_id| call_ids.iter().any(|candidate| candidate == call_id))
    {
        end += 1;
    }
    end
}

fn estimate_entries_tokens(entries: &[PromptMessageEntry]) -> usize {
    let bytes = entries
        .iter()
        .map(|entry| {
            entry.message.content.len() + entry.message.media.iter().map(String::len).sum::<usize>()
        })
        .sum::<usize>();
    estimate_tokens_from_bytes(bytes)
}

fn push_unique_item_id(ids: &mut Vec<TranscriptItemId>, item_id: TranscriptItemId) {
    if !ids.iter().any(|existing| existing == &item_id) {
        ids.push(item_id);
    }
}

fn message(role: MessageRole, content: impl Into<String>) -> Message {
    Message {
        role,
        content: content.into(),
        media: Vec::new(),
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        client_message_id: None,
        thread_id: None,
        timestamp: Utc::now(),
    }
}

fn should_keep_for_child(item: &TranscriptItem, index: usize, cutoff: usize) -> bool {
    match item.kind {
        TranscriptItemKind::SystemInstruction { .. }
        | TranscriptItemKind::DeveloperInstruction { .. }
        | TranscriptItemKind::CompactionSummary { .. } => true,
        TranscriptItemKind::UserInput { .. }
        | TranscriptItemKind::AssistantFinal { .. }
        | TranscriptItemKind::ChildResultSummary { .. } => index >= cutoff,
        TranscriptItemKind::AssistantReasoning { .. }
        | TranscriptItemKind::AssistantToolCall { .. }
        | TranscriptItemKind::ToolOutput { .. }
        | TranscriptItemKind::ContextInjection { .. }
        | TranscriptItemKind::Checkpoint { .. }
        | TranscriptItemKind::ForkBoundary { .. } => false,
    }
}

fn fork_cutoff_index(items: &[TranscriptItem], keep_last_user_turns: Option<usize>) -> usize {
    let Some(keep_last_user_turns) = keep_last_user_turns else {
        return 0;
    };
    if keep_last_user_turns == 0 {
        return items.len();
    }
    let mut seen = 0;
    for (index, item) in items.iter().enumerate().rev() {
        if matches!(item.kind, TranscriptItemKind::UserInput { .. }) {
            seen += 1;
            if seen == keep_last_user_turns {
                return index;
            }
        }
    }
    0
}

fn compaction_summary_insert_index(items: &[TranscriptItem]) -> usize {
    items
        .iter()
        .position(|item| !matches!(item.kind, TranscriptItemKind::SystemInstruction { .. }))
        .unwrap_or(items.len())
}

fn truncate_utf8(
    value: &str,
    max_bytes: usize,
    reason: ToolOutputTruncationReason,
) -> (String, Option<ToolOutputTruncationReason>) {
    if value.len() <= max_bytes {
        return (value.to_owned(), None);
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = value[..end].to_owned();
    truncated.push_str("\n[truncated]");
    (truncated, Some(reason))
}

fn tool_output_preview(value: &str) -> String {
    truncate_utf8(
        value,
        TOOL_OUTPUT_UI_PREVIEW_MAX_BYTES,
        ToolOutputTruncationReason::MaxBytes,
    )
    .0
}

fn estimate_items_tokens(items: &[TranscriptItem]) -> usize {
    let bytes = items
        .iter()
        .map(|item| {
            serde_json::to_vec(&item.kind)
                .map(|bytes| bytes.len())
                .unwrap_or_default()
        })
        .sum::<usize>();
    estimate_tokens_from_bytes(bytes)
}

fn transcript_item_kind_name(kind: &TranscriptItemKind) -> &'static str {
    match kind {
        TranscriptItemKind::SystemInstruction { .. } => "system_instruction",
        TranscriptItemKind::DeveloperInstruction { .. } => "developer_instruction",
        TranscriptItemKind::UserInput { .. } => "user_input",
        TranscriptItemKind::AssistantFinal { .. } => "assistant_final",
        TranscriptItemKind::AssistantReasoning { .. } => "assistant_reasoning",
        TranscriptItemKind::AssistantToolCall { .. } => "assistant_tool_call",
        TranscriptItemKind::ToolOutput { .. } => "tool_output",
        TranscriptItemKind::ContextInjection { .. } => "context_injection",
        TranscriptItemKind::ChildResultSummary { .. } => "child_result_summary",
        TranscriptItemKind::CompactionSummary { .. } => "compaction_summary",
        TranscriptItemKind::Checkpoint { .. } => "checkpoint",
        TranscriptItemKind::ForkBoundary { .. } => "fork_boundary",
    }
}

fn estimate_messages_tokens(messages: &[Message]) -> usize {
    let bytes = messages
        .iter()
        .map(|message| message.content.len() + message.media.iter().map(String::len).sum::<usize>())
        .sum::<usize>();
    estimate_tokens_from_bytes(bytes)
}

fn estimate_tokens_from_bytes(bytes: usize) -> usize {
    bytes.div_ceil(4).max(1)
}

fn hash_prompt_messages(messages: &[Message]) -> String {
    let stable = messages
        .iter()
        .map(|message| {
            json!({
                "role": message.role.as_str(),
                "content": message.content,
                "media": message.media,
                "tool_calls": message.tool_calls,
                "tool_call_id": message.tool_call_id,
                "reasoning_content": message.reasoning_content,
            })
        })
        .collect::<Vec<_>>();
    hash_json(&json!({ "prompt": stable }))
}

fn hash_json(value: &Value) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    sha256_prefixed(&bytes)
}

fn sha256_prefixed(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{digest:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant_tool_call(call_id: &str) -> Message {
        let mut message = Message::assistant("");
        message.tool_calls = Some(vec![ToolCall {
            id: call_id.to_owned(),
            name: "shell".to_owned(),
            arguments: json!({"cmd": "echo hi"}),
            metadata: None,
        }]);
        message
    }

    #[test]
    fn records_context_state_checkpoint_and_snapshot_hash() {
        let mut manager = ContextManager::new("coding:local:test", Some("thread-1".into()));
        // System messages are intentionally not recorded as
        // SystemInstruction items — they belong to the agent's runtime
        // prompt composition. See `record_message_with_source_ref`
        // early-return. Use User + Assistant to exercise checkpoint /
        // snapshot machinery.
        manager.record_message(&Message::user("Review this project"));
        manager.record_message(&Message::assistant("On it."));
        let before_checkpoint = manager.transcript_hash();

        let checkpoint = manager.checkpoint("before_sampling");

        assert_eq!(checkpoint.as_str(), "ctxchk_000003");
        let state = manager.state();
        assert_eq!(state.generation, 3);
        assert_eq!(state.last_checkpoint_id, Some(checkpoint));
        assert_ne!(state.transcript_hash, before_checkpoint);

        let rebuilt = ContextManager::from_snapshot(manager.snapshot());
        assert_eq!(rebuilt.state().transcript_hash, state.transcript_hash);
        assert_eq!(rebuilt.generation(), 3);
    }

    #[test]
    fn rebuilds_context_from_session_history_with_source_sequences() {
        let mut user = Message::user("hello");
        user.thread_id = Some("thread-a".into());
        let assistant =
            Message::assistant_with_thread("world", octos_core::ThreadId::new("thread-a"));
        let manager =
            ContextManager::from_session_history("coding:local:test", None, &[user, assistant]);

        assert_eq!(manager.generation(), 2);
        assert_eq!(manager.items().len(), 2);
        assert_eq!(
            manager.items()[0]
                .source_ref
                .as_ref()
                .and_then(|source| source.source_seq),
            Some(0)
        );
        assert_eq!(
            manager.items()[1]
                .source_ref
                .as_ref()
                .and_then(|source| source.source_seq),
            Some(1)
        );
        assert_eq!(
            manager.items()[1]
                .source_ref
                .as_ref()
                .and_then(|source| source.thread_id.as_deref()),
            Some("thread-a")
        );
        let source_index = manager.source_index();
        assert_eq!(source_index.len(), 2);
        assert_eq!(source_index[0].item_id, manager.items()[0].id);
        assert_eq!(source_index[0].source_seq, Some(0));
        assert_eq!(source_index[0].source_event_kind, "user");
        assert_eq!(source_index[0].transcript_item_kind, "user_input");
        assert_eq!(source_index[1].source_seq, Some(1));
        assert_eq!(source_index[1].thread_id.as_deref(), Some("thread-a"));
        assert_eq!(source_index[1].transcript_item_kind, "assistant_final");
    }

    #[test]
    fn prompt_normalization_synthesizes_missing_tool_output_and_drops_orphan() {
        let mut manager = ContextManager::new("s", None);
        manager.record_message(&assistant_tool_call("call-1"));
        manager.record_tool_output("orphan", "shell", "orphan output");

        let frame = manager.for_prompt(&PromptBuildPolicy::default());

        assert_eq!(frame.messages.len(), 2);
        assert_eq!(frame.messages[0].role, MessageRole::Assistant);
        assert_eq!(frame.messages[1].role, MessageRole::Tool);
        assert_eq!(frame.messages[1].tool_call_id.as_deref(), Some("call-1"));
        assert!(frame.messages[1].content.contains("missing"));
        assert_eq!(frame.report.synthetic_item_ids.len(), 1);
        assert_eq!(frame.report.dropped_item_ids.len(), 1);
    }

    #[test]
    fn prompt_normalization_regroups_parallel_tool_calls_before_outputs() {
        let mut manager = ContextManager::new("s", None);
        let mut assistant = Message::assistant("I'll inspect both directories.");
        assistant.tool_calls = Some(vec![
            ToolCall {
                id: "call-a".into(),
                name: "list_dir".into(),
                arguments: json!({"path": "/tmp/a"}),
                metadata: None,
            },
            ToolCall {
                id: "call-b".into(),
                name: "list_dir".into(),
                arguments: json!({"path": "/tmp/b"}),
                metadata: None,
            },
        ]);
        manager.record_message(&assistant);
        manager.record_tool_output("call-a", "list_dir", "Error: missing a");
        manager.record_tool_output("call-b", "list_dir", "Error: missing b");

        let frame = manager.for_prompt(&PromptBuildPolicy::default());

        assert_eq!(frame.messages.len(), 3);
        assert_eq!(frame.messages[0].role, MessageRole::Assistant);
        assert_eq!(frame.messages[0].content, "I'll inspect both directories.");
        let calls = frame.messages[0]
            .tool_calls
            .as_ref()
            .expect("assistant message must keep tool calls");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "call-a");
        assert_eq!(calls[1].id, "call-b");
        assert_eq!(frame.messages[1].role, MessageRole::Tool);
        assert_eq!(frame.messages[1].tool_call_id.as_deref(), Some("call-a"));
        assert_eq!(frame.messages[2].role, MessageRole::Tool);
        assert_eq!(frame.messages[2].tool_call_id.as_deref(), Some("call-b"));
        assert!(frame.report.synthetic_item_ids.is_empty());
        assert!(frame.report.dropped_item_ids.is_empty());
    }

    #[test]
    fn tool_output_envelope_truncates_and_records_raw_hash() {
        let policy = ToolOutputPolicy {
            policy_id: "test-policy".into(),
            inline_raw_threshold_bytes: 8,
            model_visible_max_bytes: 10,
        };
        let mut manager = ContextManager::new("s", None).with_tool_output_policy(policy);
        manager.record_tool_output("call-1", "shell", "0123456789abcdef");

        let envelope = match &manager.items()[0].kind {
            TranscriptItemKind::ToolOutput { envelope } => envelope,
            other => panic!("expected tool output, got {other:?}"),
        };

        assert_eq!(envelope.original_bytes, 16);
        assert_eq!(
            envelope.model_visible_bytes,
            "0123456789\n[truncated]".len()
        );
        assert_eq!(
            envelope.truncation_reason,
            Some(ToolOutputTruncationReason::MaxBytes)
        );
        assert!(
            envelope
                .raw_artifact_ref
                .as_deref()
                .unwrap()
                .starts_with("tool-output/sha256:")
        );
        let preview = envelope.ui_preview.as_ref().expect("ui preview link");
        assert_eq!(preview.preview_ref, "appui/tool-output-preview/call-1");
        assert_eq!(preview.content, envelope.model_visible_content);
        assert_eq!(preview.bytes, preview.content.len());
        assert!(envelope.raw_sha256.starts_with("sha256:"));
    }

    #[test]
    fn tool_output_envelope_inherits_real_tool_name_from_prior_assistant_call() {
        // #982: recording a Tool MessageRole used to bake the placeholder
        // tool_name "unknown" into the envelope. The envelope now resolves
        // the real name from a prior AssistantToolCall transcript entry.
        let mut manager = ContextManager::new("s", None);
        manager.record_message(&assistant_tool_call("call-7"));
        let tool_message = {
            let mut message = Message::assistant("");
            message.role = MessageRole::Tool;
            message.tool_call_id = Some("call-7".to_owned());
            message.content = "output for call-7".into();
            message
        };
        manager.record_message(&tool_message);

        let envelope = manager
            .items()
            .iter()
            .find_map(|item| match &item.kind {
                TranscriptItemKind::ToolOutput { envelope } => Some(envelope),
                _ => None,
            })
            .expect("recorded tool output");
        assert_eq!(envelope.tool_call_id, "call-7");
        assert_eq!(envelope.tool_name, "shell");
    }

    #[test]
    fn tool_output_envelope_resolves_real_tool_name_through_persisted_merge_path() {
        // #982: the merging-prompt-equivalent path runs a fresh probe
        // ContextManager and so cannot resolve the prior AssistantToolCall
        // on its own. Verify the post-probe patch-up still wires the real
        // tool_name onto the merged envelope.
        let mut manager = ContextManager::new("s", None);
        manager.record_persisted_message(&assistant_tool_call("call-9"), 0);

        let tool_message = {
            let mut message = Message::assistant("");
            message.role = MessageRole::Tool;
            message.tool_call_id = Some("call-9".to_owned());
            message.content = "merged output for call-9".into();
            message
        };
        manager.record_persisted_message_merging_prompt_equivalent(&tool_message, 1);

        let envelope = manager
            .items()
            .iter()
            .find_map(|item| match &item.kind {
                TranscriptItemKind::ToolOutput { envelope } => Some(envelope),
                _ => None,
            })
            .expect("persisted merge produced tool output");
        assert_eq!(envelope.tool_name, "shell");
    }

    #[test]
    fn tool_output_envelope_falls_back_to_unknown_without_prior_tool_call() {
        // #982: when no prior AssistantToolCall is present (orphan tool
        // message), the envelope still records the output with the
        // legacy "unknown" name so replay does not lose data.
        let mut manager = ContextManager::new("s", None);
        let tool_message = {
            let mut message = Message::assistant("");
            message.role = MessageRole::Tool;
            message.tool_call_id = Some("orphan-call".to_owned());
            message.content = "orphan output".into();
            message
        };
        manager.record_message(&tool_message);
        let envelope = manager
            .items()
            .iter()
            .find_map(|item| match &item.kind {
                TranscriptItemKind::ToolOutput { envelope } => Some(envelope),
                _ => None,
            })
            .expect("orphan tool output");
        assert_eq!(envelope.tool_name, "unknown");
    }

    /// #1022 — parent prompt generation must surface a child result as
    /// a bounded `ChildResultSummary` capsule only. The rendered prompt
    /// entry must be a single assistant message of the form
    /// `[child <id> summary]\n<summary>\nArtifacts: <refs>`, with no
    /// other content slipping in from the child's raw transcript.
    #[test]
    fn child_result_summary_renders_as_bounded_assistant_capsule() {
        let mut manager = ContextManager::new("s", None);
        manager.record_message(&Message::system("system"));
        manager.record_message(&Message::user("review the diff"));

        manager.record_item(
            TranscriptItemKind::ChildResultSummary {
                child_agent_id: "reviewer-42".to_owned(),
                summary: "one P0 finding: missing null check in foo.rs:12".to_owned(),
                artifact_refs: vec![
                    "agent/reviewer-42/finding.md".to_owned(),
                    "agent/reviewer-42/diff.patch".to_owned(),
                ],
                reference_artifact_refs: vec![],
            },
            TranscriptItemSource::Supervisor,
        );

        let frame = manager.for_prompt(&PromptBuildPolicy::default());
        let capsule = frame
            .messages
            .iter()
            .find(|m| m.content.starts_with("[child reviewer-42 summary]"))
            .expect("child summary message");
        assert_eq!(capsule.role, MessageRole::Assistant);
        assert!(
            capsule.content.contains("one P0 finding"),
            "summary text must be present"
        );
        assert!(
            capsule
                .content
                .contains("Artifacts: agent/reviewer-42/finding.md, agent/reviewer-42/diff.patch"),
            "artifact refs must be rendered as a bounded comma-separated list"
        );
        // Nothing else from a "child world" should leak into the capsule
        // — the rendering takes only `summary` and `artifact_refs`.
        assert!(!capsule.content.contains("system"));
        assert!(!capsule.content.contains("review the diff"));
        assert!(capsule.tool_calls.is_none());
        assert!(capsule.tool_call_id.is_none());
    }

    /// #1022 — even when the child agent had a verbose transcript with
    /// system instructions, user messages, reasoning, tool calls, and
    /// raw tool output, the parent context — once the supervisor records
    /// only a `ChildResultSummary` — must NOT contain any of that raw
    /// child content in its prompt generation output.
    ///
    /// Guards against a future change that, e.g., copies the child's
    /// full transcript into the parent's `ChildResultSummary.summary`
    /// field (the parent prompt would still render it, which is exactly
    /// the pollution this issue forbids — but the join layer is the
    /// right place to enforce bounded summaries).
    #[test]
    fn parent_prompt_contains_no_raw_child_transcript_when_only_summary_is_recorded() {
        // Build a "child" transcript in a separate ContextManager to
        // simulate the child agent's local view. We never copy these
        // items into the parent — the parent only receives a bounded
        // result capsule.
        let mut child = ContextManager::new("child", None);
        child.record_message(&Message::system("you are a code reviewer"));
        child.record_message(&Message::user("review main.rs"));
        let mut child_assistant = Message::assistant("");
        child_assistant.reasoning_content = Some("scanning main.rs for issues".into());
        child_assistant.tool_calls = Some(vec![ToolCall {
            id: "child-call-1".into(),
            name: "read_file".into(),
            arguments: json!({"path": "main.rs"}),
            metadata: None,
        }]);
        child.record_message(&child_assistant);
        child.record_tool_output(
            "child-call-1",
            "read_file",
            "fn main() { let secret = ...; }",
        );

        // Parent records only a bounded summary derived from the child.
        let mut parent = ContextManager::new("parent", None);
        parent.record_message(&Message::system("you are the supervisor"));
        parent.record_message(&Message::user("kick off the reviewer"));
        parent.record_item(
            TranscriptItemKind::ChildResultSummary {
                child_agent_id: "reviewer".to_owned(),
                summary: "Reviewer found 1 issue. See finding.md.".to_owned(),
                artifact_refs: vec!["agent/reviewer/finding.md".to_owned()],
                reference_artifact_refs: vec![],
            },
            TranscriptItemSource::Supervisor,
        );

        let frame = parent.for_prompt(&PromptBuildPolicy::default());
        let parent_blob: String = frame
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n---\n");

        // Bounded summary IS present.
        assert!(parent_blob.contains("Reviewer found 1 issue"));
        assert!(parent_blob.contains("agent/reviewer/finding.md"));
        // Raw child content must NOT leak — none of the strings the
        // child agent wrote in its own transcript should appear in
        // the parent's prompt.
        assert!(
            !parent_blob.contains("you are a code reviewer"),
            "child's system instruction must not appear in parent prompt"
        );
        assert!(
            !parent_blob.contains("review main.rs"),
            "child's user message must not appear in parent prompt"
        );
        assert!(
            !parent_blob.contains("scanning main.rs for issues"),
            "child's reasoning must not appear in parent prompt"
        );
        assert!(
            !parent_blob.contains("fn main() { let secret"),
            "child's raw tool output must not appear in parent prompt"
        );
        // The parent's own messages also retain no child tool-call
        // ids — verify the assistant capsule has no tool_calls field.
        let capsule = frame
            .messages
            .iter()
            .find(|m| m.content.contains("Reviewer found 1 issue"))
            .expect("parent must render the result capsule");
        assert_eq!(capsule.role, MessageRole::Assistant);
        assert!(capsule.tool_calls.is_none());
    }

    /// #1022 / M17-D — reference-join artifact refs must render as a
    /// separate `References:` line in the prompt, distinct from the
    /// `Artifacts:` owned-list. This pins the pointer-vs-ownership
    /// distinction at the rendering layer so future code that copies
    /// references into the owned list would surface as a test failure.
    #[test]
    fn child_result_summary_renders_reference_refs_separately_from_owned() {
        let mut manager = ContextManager::new("s", None);
        manager.record_item(
            TranscriptItemKind::ChildResultSummary {
                child_agent_id: "reviewer-99".to_owned(),
                summary: "merged finding".to_owned(),
                artifact_refs: vec!["agent/reviewer-99/owned.md".to_owned()],
                reference_artifact_refs: vec![
                    "agent/sibling-1/ref-a.md".to_owned(),
                    "agent/sibling-2/ref-b.md".to_owned(),
                ],
            },
            TranscriptItemSource::Supervisor,
        );

        let frame = manager.for_prompt(&PromptBuildPolicy::default());
        let capsule = frame
            .messages
            .iter()
            .find(|m| m.content.starts_with("[child reviewer-99 summary]"))
            .expect("child summary message");
        assert!(
            capsule
                .content
                .contains("Artifacts: agent/reviewer-99/owned.md")
        );
        assert!(
            capsule
                .content
                .contains("References: agent/sibling-1/ref-a.md, agent/sibling-2/ref-b.md")
        );
        // Critically: the two lines must be separate. Reference refs
        // must NOT appear inside the Artifacts list.
        assert!(
            !capsule
                .content
                .contains("Artifacts: agent/reviewer-99/owned.md, agent/sibling-1/ref-a.md")
        );
    }

    /// #1022 / M17-D — when a summary has only references and no
    /// owned artifacts, the prompt must still render the References:
    /// line (and must NOT pretend it's an Artifacts: list).
    #[test]
    fn child_result_summary_renders_pure_reference_join() {
        let mut manager = ContextManager::new("s", None);
        manager.record_item(
            TranscriptItemKind::ChildResultSummary {
                child_agent_id: "browser".to_owned(),
                summary: "pointer-only join".to_owned(),
                artifact_refs: vec![],
                reference_artifact_refs: vec!["agent/sibling/ref.md".to_owned()],
            },
            TranscriptItemSource::Supervisor,
        );

        let frame = manager.for_prompt(&PromptBuildPolicy::default());
        let capsule = frame
            .messages
            .iter()
            .find(|m| m.content.starts_with("[child browser summary]"))
            .expect("child summary message");
        assert!(capsule.content.contains("References: agent/sibling/ref.md"));
        // The Artifacts: prefix must NOT appear at all when only
        // references are present — the parent did not gain ownership.
        assert!(!capsule.content.contains("Artifacts:"));
    }

    /// #1022 / M17-D — older snapshots (and migration shims) that
    /// omit `reference_artifact_refs` entirely must round-trip cleanly
    /// thanks to `#[serde(default)]`, and must render the same prompt
    /// shape as before (no spurious References: line).
    #[test]
    fn child_result_summary_legacy_snapshot_without_reference_refs_round_trips() {
        let legacy = serde_json::json!({
            "type": "child_result_summary",
            "child_agent_id": "legacy",
            "summary": "old",
            "artifact_refs": ["agent/legacy/old.md"]
        });
        let kind: TranscriptItemKind =
            serde_json::from_value(legacy).expect("legacy ChildResultSummary deserializes");
        let mut manager = ContextManager::new("s", None);
        manager.record_item(kind, TranscriptItemSource::Supervisor);

        let frame = manager.for_prompt(&PromptBuildPolicy::default());
        let capsule = frame
            .messages
            .iter()
            .find(|m| m.content.starts_with("[child legacy summary]"))
            .expect("legacy child summary message");
        assert!(capsule.content.contains("Artifacts: agent/legacy/old.md"));
        assert!(!capsule.content.contains("References:"));
    }

    /// #1022 / M17-D — the production writer
    /// `record_child_result_summary` is the canonical fold point for the
    /// parent session's join. It must:
    ///   1. produce a `Supervisor`-sourced transcript item,
    ///   2. round-trip through `for_prompt` into a bounded assistant
    ///      capsule with the documented `[child <id> summary]` shape,
    ///   3. carry both owned `Artifacts:` and pointer-only `References:`
    ///      lines when the join is mixed,
    ///   4. surface ZERO raw child transcript content into the parent's
    ///      prompt (the writer's whole job is to act as the bounded
    ///      capsule that prevents that leakage).
    ///
    /// This pins the writer to the SubagentResultCapsule contract end
    /// to end: caller -> ContextManager -> rendered prompt.
    #[test]
    fn record_child_result_summary_writes_supervisor_sourced_bounded_capsule() {
        let mut manager = ContextManager::new("parent-session", None);
        manager.record_message(&Message::system("parent system prompt"));
        manager.record_message(&Message::user("please review the change"));

        let id = manager.record_child_result_summary(
            "reviewer-7",
            "found 1 P0: missing null check in foo.rs:12",
            vec!["agent/reviewer-7/finding.md".to_owned()],
            vec!["agent/sibling-archive/prior-finding.md".to_owned()],
        );

        // The writer must produce a Supervisor-sourced item with the
        // requested kind.
        let item = manager
            .items()
            .iter()
            .find(|item| item.id == id)
            .expect("writer-produced item");
        assert_eq!(item.source, TranscriptItemSource::Supervisor);
        match &item.kind {
            TranscriptItemKind::ChildResultSummary {
                child_agent_id,
                summary,
                artifact_refs,
                reference_artifact_refs,
            } => {
                assert_eq!(child_agent_id, "reviewer-7");
                assert!(summary.contains("missing null check"));
                assert_eq!(
                    artifact_refs,
                    &vec!["agent/reviewer-7/finding.md".to_owned()]
                );
                assert_eq!(
                    reference_artifact_refs,
                    &vec!["agent/sibling-archive/prior-finding.md".to_owned()]
                );
            }
            other => panic!("expected ChildResultSummary, got {other:?}"),
        }

        // for_prompt renders the capsule as a single bounded assistant
        // message — and absolutely nothing else from the child world.
        let frame = manager.for_prompt(&PromptBuildPolicy::default());
        let capsule = frame
            .messages
            .iter()
            .find(|m| m.content.starts_with("[child reviewer-7 summary]"))
            .expect("rendered child summary capsule");
        assert_eq!(capsule.role, MessageRole::Assistant);
        assert!(capsule.content.contains("missing null check"));
        assert!(
            capsule
                .content
                .contains("Artifacts: agent/reviewer-7/finding.md")
        );
        assert!(
            capsule
                .content
                .contains("References: agent/sibling-archive/prior-finding.md")
        );
        // Owned-vs-reference separation: a reference must never appear
        // inside the Artifacts: list.
        assert!(
            !capsule
                .content
                .contains("Artifacts: agent/reviewer-7/finding.md, agent/sibling-archive")
        );
        assert!(capsule.tool_calls.is_none());
        assert!(capsule.tool_call_id.is_none());
    }

    /// codex P2 follow-up to #1111 — the legacy form of a
    /// `ChildResultSummary` snapshot did NOT contain
    /// `reference_artifact_refs` at all. The post-#1111 in-memory form
    /// has an empty `Vec<String>` for the same shape.
    ///
    /// Without `#[serde(skip_serializing_if = "Vec::is_empty")]` on
    /// `reference_artifact_refs`, `StableTranscriptHashItem` would
    /// serialize `"reference_artifact_refs": []` into the canonical
    /// hash bytes, drifting the transcript hash away from any pre-#1111
    /// snapshot. This test pins that the hash of an
    /// in-memory-constructed summary (empty references) is identical to
    /// the hash of the same summary parsed from a legacy JSON snapshot
    /// that lacked the field entirely.
    #[test]
    fn transcript_hash_stable_for_empty_reference_artifact_refs() {
        // (1) In-memory form: empty Vec for the new field.
        let mut modern = ContextManager::new("s", None);
        modern.record_item(
            TranscriptItemKind::ChildResultSummary {
                child_agent_id: "agent-x".to_owned(),
                summary: "ok".to_owned(),
                artifact_refs: vec!["agent/x/artifact.md".to_owned()],
                reference_artifact_refs: vec![],
            },
            TranscriptItemSource::Supervisor,
        );

        // (2) Legacy form: same summary deserialized from JSON that
        // omits `reference_artifact_refs` entirely.
        let legacy_kind: TranscriptItemKind = serde_json::from_value(serde_json::json!({
            "type": "child_result_summary",
            "child_agent_id": "agent-x",
            "summary": "ok",
            "artifact_refs": ["agent/x/artifact.md"]
        }))
        .expect("legacy ChildResultSummary deserializes");
        let mut legacy = ContextManager::new("s", None);
        legacy.record_item(legacy_kind, TranscriptItemSource::Supervisor);

        // Hashes are computed over `StableTranscriptHashItem`, which
        // serializes `kind` via the canonical `TranscriptItemKind`
        // serde. With `skip_serializing_if = "Vec::is_empty"`, both
        // forms canonicalize to the same JSON and therefore the same
        // sha256.
        assert_eq!(
            modern.transcript_hash(),
            legacy.transcript_hash(),
            "empty reference_artifact_refs must hash identically to a legacy snapshot that omits the field"
        );
    }

    #[test]
    fn durable_context_ledger_persists_tool_output_sidecar_and_preview_link() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = "coding:local:tui#tool-output";
        let policy = ToolOutputPolicy {
            policy_id: "test-policy".into(),
            inline_raw_threshold_bytes: 8,
            model_visible_max_bytes: 10,
        };
        let mut manager = ContextManager::new(session_id, None).with_tool_output_policy(policy);
        manager.record_message(&assistant_tool_call("call-1"));
        manager.record_tool_output("call-1", "shell", "0123456789abcdef");

        let envelope = match &manager.items()[1].kind {
            TranscriptItemKind::ToolOutput { envelope } => envelope,
            other => panic!("expected tool output, got {other:?}"),
        };
        let artifact_ref = envelope
            .raw_artifact_ref
            .as_deref()
            .expect("large output should have sidecar ref");
        let preview_ref = envelope
            .ui_preview
            .as_ref()
            .expect("ui preview link")
            .preview_ref
            .clone();

        let snapshot_path = persist_context_manager_snapshot(temp.path(), session_id, &manager)
            .expect("persist context manager");
        let artifact_path =
            context_ledger_artifact_path(temp.path(), artifact_ref).expect("artifact path");

        assert!(snapshot_path.exists());
        assert_eq!(
            std::fs::read_to_string(&artifact_path).expect("read sidecar"),
            "0123456789abcdef"
        );
        assert_eq!(preview_ref, "appui/tool-output-preview/call-1");

        let loaded = load_context_manager_snapshot(temp.path(), session_id)
            .expect("load snapshot")
            .expect("snapshot exists");
        let frame = loaded.for_prompt(&PromptBuildPolicy::default());
        let tool_message = frame
            .messages
            .iter()
            .find(|message| message.role == MessageRole::Tool)
            .expect("tool message");
        assert_eq!(tool_message.content, "0123456789\n[truncated]");
        assert_eq!(
            frame.report.truncated_item_ids.len(),
            1,
            "replay should preserve the same model-visible truncation evidence"
        );
    }

    #[test]
    fn prompt_context_pressure_truncates_tool_output_before_dropping_groups() {
        let policy = ToolOutputPolicy {
            policy_id: "test-policy".into(),
            inline_raw_threshold_bytes: 1024,
            model_visible_max_bytes: 1024,
        };
        let mut manager = ContextManager::new("s", None).with_tool_output_policy(policy);
        manager.record_message(&Message::system("system"));
        manager.record_message(&Message::user("recent user"));
        manager.record_message(&assistant_tool_call("call-pressure"));
        let tool_item_id = manager.record_tool_output("call-pressure", "shell", &"x".repeat(200));

        let frame = manager.for_prompt(&PromptBuildPolicy {
            max_prompt_token_estimate: Some(40),
            ..PromptBuildPolicy::default()
        });

        let tool_message = frame
            .messages
            .iter()
            .find(|message| message.role == MessageRole::Tool)
            .expect("tool output should remain");
        assert!(tool_message.content.ends_with("[truncated]"));
        assert!(tool_message.content.len() < 200);
        assert!(
            frame.report.truncated_item_ids.contains(&tool_item_id),
            "context-pressure truncation should report the affected tool output item"
        );
    }

    #[test]
    fn fork_child_history_drops_parent_reasoning_tool_calls_outputs_and_context_injections() {
        let mut manager = ContextManager::new("s", None);
        manager.record_message(&Message::system("system"));
        manager.record_message(&Message::user("first"));
        let mut assistant = Message::assistant("done");
        assistant.reasoning_content = Some("private reasoning".into());
        manager.record_message(&assistant);
        manager.record_message(&assistant_tool_call("call-1"));
        manager.record_tool_output("call-1", "shell", "secret tool output");
        manager.record_item(
            TranscriptItemKind::ContextInjection {
                label: "parent".into(),
                content: "parent-only baseline".into(),
            },
            TranscriptItemSource::AgentLoop,
        );
        manager.record_message(&Message::user("second"));
        manager.record_message(&Message::assistant("second done"));

        let fork = manager.fork_child_history(&ForkPolicy {
            policy_id: "test-fork".into(),
            keep_last_user_turns: Some(1),
        });
        let kinds = fork
            .items
            .iter()
            .map(|item| std::mem::discriminant(&item.kind))
            .collect::<Vec<_>>();

        assert!(fork.sanitizer_hash.starts_with("sha256:"));
        // SystemInstruction items are no longer recorded into the
        // manager (see `record_message_with_source_ref` early-return)
        // — the agent re-applies its runtime System at the bridge.
        // The fork therefore never carries a SystemInstruction either.
        assert!(
            !fork
                .items
                .iter()
                .any(|item| matches!(item.kind, TranscriptItemKind::SystemInstruction { .. }))
        );
        assert!(fork.items.iter().any(|item| matches!(item.kind, TranscriptItemKind::UserInput { ref content, .. } if content == "second")));
        assert!(fork.items.iter().any(|item| matches!(item.kind, TranscriptItemKind::AssistantFinal { ref content } if content == "second done")));
        assert!(
            fork.items
                .iter()
                .any(|item| matches!(item.kind, TranscriptItemKind::ForkBoundary { .. }))
        );
        assert!(!kinds.iter().any(|kind| *kind
            == std::mem::discriminant(&TranscriptItemKind::AssistantReasoning {
                content: String::new()
            })));
        assert!(!kinds.iter().any(|kind| *kind
            == std::mem::discriminant(&TranscriptItemKind::AssistantToolCall {
                call_id: String::new(),
                name: String::new(),
                arguments: Value::Null
            })));
        assert!(!kinds.iter().any(|kind| *kind
            == std::mem::discriminant(&TranscriptItemKind::ToolOutput {
                envelope: ToolOutputEnvelope {
                    tool_call_id: String::new(),
                    tool_name: String::new(),
                    raw_sha256: String::new(),
                    raw_artifact_ref: None,
                    ui_preview: None,
                    original_bytes: 0,
                    model_visible_content: String::new(),
                    model_visible_bytes: 0,
                    truncation_reason: None,
                    policy_id: String::new(),
                }
            })));
        assert!(
            !fork
                .items
                .iter()
                .any(|item| matches!(item.kind, TranscriptItemKind::ContextInjection { .. }))
        );
    }

    #[test]
    fn compaction_summary_advances_generation_and_reduces_prompt_history() {
        let mut manager = ContextManager::new("s", None);
        manager.record_message(&Message::system("system"));
        for index in 0..6 {
            manager.record_message(&Message::user(format!("u{index}")));
            manager.record_message(&Message::assistant(format!("a{index}")));
        }
        let before_generation = manager.generation();

        let compaction_id = manager.install_compaction_summary("older turns summarized", 2);

        assert_eq!(manager.state().last_compaction_id, Some(compaction_id));
        assert_eq!(manager.generation(), before_generation + 1);
        assert!(
            manager
                .items()
                .iter()
                .any(|item| matches!(item.kind, TranscriptItemKind::CompactionSummary { .. }))
        );
        let prompt = manager.for_prompt(&PromptBuildPolicy::default());
        assert!(
            prompt
                .messages
                .iter()
                .any(|message| message.content.contains("Conversation summary"))
        );
        assert!(
            prompt
                .messages
                .iter()
                .any(|message| message.content == "u5")
        );
        assert!(
            prompt
                .messages
                .iter()
                .any(|message| message.content == "a5")
        );
        assert!(
            !prompt
                .messages
                .iter()
                .any(|message| message.content == "u0")
        );
    }

    #[test]
    fn compact_context_records_lifecycle_evidence_and_installed_generation() {
        let mut manager = ContextManager::new("s", None);
        manager.record_message(&Message::system("system"));
        for index in 0..5 {
            manager.record_message(&Message::user(format!("u{index}")));
            manager.record_message(&Message::assistant(format!("a{index}")));
        }
        let input_generation = manager.generation();
        let input_hash = manager.transcript_hash();

        let record = manager.compact_context(
            "older turns summarized",
            CompactContextPolicy {
                policy_id: "test-compact".into(),
                trigger: "context_pressure".into(),
                keep_recent_items: 2,
                preserve_system_instructions: true,
            },
        );

        assert_eq!(record.status, ContextCompactionStatus::Installed);
        assert_eq!(record.policy_id, "test-compact");
        assert_eq!(record.trigger, "context_pressure");
        assert_eq!(record.input_generation, input_generation);
        assert_eq!(record.output_generation, Some(input_generation + 1));
        assert_eq!(record.input_transcript_hash, input_hash);
        assert!(record.replacement_transcript_hash.is_some());
        assert_eq!(
            record.installed_transcript_hash.as_deref(),
            Some(manager.transcript_hash().as_str())
        );
        assert!(record.summary_item_id.is_some());
        assert!(!record.retained_item_ids.is_empty());
        assert!(!record.dropped_item_ids.is_empty());
        assert_eq!(manager.compactions().len(), 1);
        assert_eq!(
            manager.state().last_compaction_id,
            Some(record.compaction_id)
        );
        assert_eq!(
            manager.state().last_checkpoint_id,
            Some(record.checkpoint_id)
        );
    }

    #[test]
    fn failed_compaction_records_evidence_without_mutating_active_generation() {
        let mut manager = ContextManager::new("s", None);
        manager.record_message(&Message::system("system"));
        manager.record_message(&Message::user("hello"));
        let input_generation = manager.generation();
        let input_hash = manager.transcript_hash();
        let input_items = manager.items().to_vec();

        let record =
            manager.record_failed_compaction(CompactContextPolicy::default(), "model overloaded");

        assert_eq!(record.status, ContextCompactionStatus::Failed);
        assert_eq!(record.input_generation, input_generation);
        assert_eq!(record.output_generation, None);
        assert_eq!(record.input_transcript_hash, input_hash);
        assert_eq!(record.error.as_deref(), Some("model overloaded"));
        assert_eq!(manager.generation(), input_generation);
        assert_eq!(manager.items(), input_items.as_slice());
        assert!(manager.state().last_compaction_id.is_none());
        assert_eq!(manager.compactions().len(), 1);
    }

    #[test]
    fn snapshot_preserves_compaction_records() {
        let mut manager = ContextManager::new("s", None);
        manager.record_message(&Message::system("system"));
        manager.record_message(&Message::user("hello"));
        manager.compact_context("summary", CompactContextPolicy::default());

        let rebuilt = ContextManager::from_snapshot(manager.snapshot());

        assert_eq!(rebuilt.compactions(), manager.compactions());
        assert_eq!(
            rebuilt.state().last_compaction_id,
            manager.state().last_compaction_id
        );
    }

    #[test]
    fn durable_context_ledger_round_trips_active_compacted_generation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = "coding:local:tui#coding";
        // System messages are intentionally skipped by
        // `record_message_with_source_ref` (the agent re-applies its
        // runtime System at the bridge), so seed history with
        // user/assistant pairs only.
        let mut history = vec![];
        for index in 0..6 {
            history.push(Message::user(format!("u{index}")));
            history.push(Message::assistant(format!("a{index}")));
        }
        let mut manager = ContextManager::from_session_history(session_id, None, &history);
        manager.compact_context(
            "older turns summarized",
            CompactContextPolicy {
                trigger: "test_context_pressure".into(),
                keep_recent_items: 4,
                ..CompactContextPolicy::default()
            },
        );
        let generation = manager.generation();
        let transcript_hash = manager.transcript_hash();

        let path = persist_context_manager_snapshot(temp.path(), session_id, &manager)
            .expect("persist snapshot");
        assert!(path.exists());
        let snapshot_json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read persisted snapshot"))
                .expect("parse persisted snapshot");
        // source_index[0] anchors at the source_seq of the first item
        // retained AFTER compaction (keep_recent_items: 4 above keeps
        // the last 4 messages of the user/assistant pairs). With
        // System messages now intentionally skipped at record time
        // (`record_message_with_source_ref` early-return), 12 items go
        // in (u0..a5) at source_seqs 0..11; compaction keeps the last
        // 4 (source_seqs 8..11), so the snapshot's source_index begins
        // at 8 rather than 0. The atomic materialization invariant
        // still holds — the source_index is present and consistent.
        assert_eq!(
            snapshot_json["source_index"][0]["source_seq"],
            serde_json::json!(8),
            "context snapshot must atomically materialize the normalized source index"
        );

        let (loaded, status) =
            load_or_rebuild_context_manager(temp.path(), session_id, None, &history);

        assert_eq!(status, ContextLedgerLoadStatus::Loaded);
        assert_eq!(loaded.generation(), generation);
        assert_eq!(loaded.transcript_hash(), transcript_hash);
        assert_eq!(loaded.compactions().len(), 1);
        assert_eq!(loaded.state().recovery_state, ContextRecoveryState::Exact);
        let prompt = loaded.for_prompt(&PromptBuildPolicy::default());
        assert_eq!(
            prompt.context_state.generation, generation,
            "reload prompt must use the compacted active generation"
        );
        assert_eq!(
            prompt.context_state.transcript_hash, transcript_hash,
            "reload prompt must reference the persisted compacted transcript"
        );
        assert!(
            prompt
                .messages
                .iter()
                .any(|message| message.content.contains("[Conversation summary]")
                    && message.content.contains("older turns summarized")),
            "reload prompt must preserve the compact-context summary frame"
        );
        assert!(
            prompt.messages.len() < history.len(),
            "reload prompt should be compacted, not rebuilt from full raw history"
        );
    }

    #[test]
    fn durable_context_ledger_rebuilds_when_snapshot_is_stale() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = "coding:local:tui#coding";
        let short_history = vec![Message::user("first")];
        let full_history = vec![Message::user("first"), Message::assistant("second")];
        let manager = ContextManager::from_session_history(session_id, None, &short_history);
        persist_context_manager_snapshot(temp.path(), session_id, &manager)
            .expect("persist stale snapshot");

        let (rebuilt, status) =
            load_or_rebuild_context_manager(temp.path(), session_id, None, &full_history);

        assert_eq!(status, ContextLedgerLoadStatus::Stale);
        assert_eq!(
            rebuilt.state().recovery_state,
            ContextRecoveryState::Rebuilt
        );
        assert_eq!(rebuilt.source_high_watermark(), Some(1));
        assert!(rebuilt.compactions().is_empty());
        assert!(
            rebuilt
                .for_prompt(&PromptBuildPolicy::default())
                .messages
                .iter()
                .any(|message| message.content == "second")
        );
    }

    #[test]
    fn persisted_message_merge_stamps_prompt_equivalent_without_duplication() {
        let mut manager = ContextManager::new("coding:local:test", None);
        manager.record_message(&Message::system("system"));
        manager.record_message(&Message::user("current turn"));
        let before_len = manager.items().len();

        let ids = manager
            .record_persisted_message_merging_prompt_equivalent(&Message::user("current turn"), 7);

        assert_eq!(ids.len(), 1);
        assert_eq!(
            manager.items().len(),
            before_len,
            "merging the durable row should not duplicate a prompt-only item"
        );
        assert_eq!(manager.source_high_watermark(), Some(7));
        let prompt = manager.for_prompt(&PromptBuildPolicy::default());
        assert_eq!(
            prompt
                .messages
                .iter()
                .filter(|message| message.role == MessageRole::User
                    && message.content == "current turn")
                .count(),
            1
        );
    }

    #[test]
    fn durable_snapshot_load_then_merge_preserves_prompt_equivalent_current_turn() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = "coding:local:tui#coding";
        let mut manager = ContextManager::new(session_id, None);
        manager.record_message(&Message::system("system"));
        manager.record_message(&Message::user("current turn"));
        persist_context_manager_snapshot(temp.path(), session_id, &manager)
            .expect("persist prompt scratch snapshot");

        let mut loaded = load_context_manager_snapshot(temp.path(), session_id)
            .expect("load snapshot")
            .expect("snapshot exists");
        let before_len = loaded.items().len();
        loaded
            .record_persisted_message_merging_prompt_equivalent(&Message::user("current turn"), 3);

        assert_eq!(
            loaded.items().len(),
            before_len,
            "durable prompt scratch rows should be stamped, not duplicated"
        );
        assert_eq!(loaded.source_high_watermark(), Some(3));
        let prompt = loaded.for_prompt(&PromptBuildPolicy::default());
        assert_eq!(
            prompt
                .messages
                .iter()
                .filter(|message| {
                    message.role == MessageRole::User && message.content == "current turn"
                })
                .count(),
            1
        );
    }

    #[test]
    fn media_is_stripped_when_model_capability_is_text_only() {
        let mut manager = ContextManager::new("s", None);
        let mut user = Message::user("inspect image");
        user.media = vec!["image.png".into()];
        manager.record_message(&user);

        let text_only = manager.for_prompt(&PromptBuildPolicy::default());
        let media_model = manager.for_prompt(&PromptBuildPolicy {
            supports_media: true,
            model_capability_id: "vision-v1".into(),
            ..PromptBuildPolicy::default()
        });

        assert!(text_only.messages[0].media.is_empty());
        assert_eq!(text_only.report.repaired_item_ids.len(), 1);
        assert_eq!(media_model.messages[0].media, vec!["image.png"]);
        assert!(media_model.report.repaired_item_ids.is_empty());
    }

    #[test]
    fn prompt_normalization_is_idempotent_for_same_policy() {
        let mut manager = ContextManager::new("s", None);
        manager.record_message(&Message::system("system"));
        manager.record_message(&Message::user("hello"));
        manager.record_message(&Message::assistant("world"));

        let first = manager.for_prompt(&PromptBuildPolicy::default());
        let second = manager.for_prompt(&PromptBuildPolicy::default());

        assert_eq!(
            first.report.output_prompt_hash,
            second.report.output_prompt_hash
        );
        assert_eq!(
            first.report.dropped_item_ids,
            second.report.dropped_item_ids
        );
        assert_eq!(first.messages.len(), second.messages.len());
    }

    #[test]
    fn prompt_token_trim_preserves_system_and_tool_call_output_pairs() {
        let mut manager = ContextManager::new("s", None);
        manager.record_message(&Message::system("system"));
        manager.record_message(&Message::user("old user ".repeat(80)));
        manager.record_message(&Message::assistant("old answer ".repeat(80)));
        manager.record_message(&Message::user("recent user"));
        manager.record_message(&assistant_tool_call("call_keep"));
        manager.record_message(&Message::tool_with_thread(
            "tool result",
            "call_keep",
            octos_core::ThreadId::new("thread-1"),
        ));

        let frame = manager.for_prompt(&PromptBuildPolicy {
            max_prompt_token_estimate: Some(32),
            ..PromptBuildPolicy::default()
        });

        // System messages are no longer owned by the manager (see
        // `record_message_with_source_ref` early-return) — the agent's
        // runtime System is re-applied at the bridge boundary instead.
        // Frame.messages therefore should NOT contain a System.
        assert!(
            !frame
                .messages
                .iter()
                .any(|message| message.role == MessageRole::System)
        );
        assert!(
            !frame
                .messages
                .iter()
                .any(|message| message.content.contains("old user"))
        );
        assert!(
            frame
                .messages
                .iter()
                .any(|message| message.content == "recent user")
        );
        assert!(
            frame.messages.iter().any(|message| {
                message
                    .tool_calls
                    .as_ref()
                    .is_some_and(|calls| calls.iter().any(|call| call.id == "call_keep"))
            }),
            "tool call should remain when its result remains"
        );
        assert!(
            frame.messages.iter().any(|message| {
                message.role == MessageRole::Tool
                    && message.tool_call_id.as_deref() == Some("call_keep")
            }),
            "tool output should remain with its tool call"
        );
        assert!(
            frame.report.dropped_item_ids.len() >= 2,
            "trimmed prompt items should be reported as dropped"
        );
    }
}
