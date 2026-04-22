//! Context compaction for fitting conversation history into context windows.
//!
//! Two layers live in this module:
//!
//! 1. Legacy extractive helpers ([`compact_messages`], [`find_recent_boundary`],
//!    etc.) — deterministic, budget-aware, tool-call safe. Used by the
//!    `Agent::trim_to_context_window` path and by the [`ExtractiveSummarizer`]
//!    fallback. Behaviour is preserved verbatim so pre-M6.3 tests still pass.
//!
//! 2. [`CompactionRunner`] + [`CompactionPolicy`] (harness M6.3) — declarative
//!    compaction with preserved artifacts/invariants, preflight triggering,
//!    typed [`ToolResultPlaceholder`]s, and `octos.harness.event.v1 { kind:
//!    phase, phase: "compaction" }` emission. Swappable summarizer via the
//!    [`crate::summarizer::Summarizer`] trait.
//!
//! The runner is intentionally synchronous so the agent loop can run it
//! inline without awaiting. LLM-iterative summarizers (M6.4) can drive a
//! `tokio::runtime::Handle::current().block_on()` call from their
//! [`Summarizer::summarize`] impl.

use std::path::Path;
use std::sync::Arc;

use chrono::Utc;
use octos_core::{Message, MessageRole};
use octos_llm::context::{estimate_message_tokens, estimate_tokens};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::abi_schema::COMPACTION_POLICY_SCHEMA_VERSION;
use crate::harness_events::{HarnessEvent, write_event_to_sink};
pub use crate::summarizer::{ExtractiveSummarizer, Summarizer};
pub use crate::workspace_policy::{CompactionPolicy, CompactionSummarizerKind};
use crate::workspace_policy::{WorkspaceArtifactsPolicy, WorkspacePolicy};

// ---------------------------------------------------------------------------
// Legacy extractive helpers (preserved verbatim from M0).
// ---------------------------------------------------------------------------

/// Safety margin multiplier for token estimation inaccuracy.
pub(crate) const SAFETY_MARGIN: f64 = 1.2;

/// Minimum non-system messages to always keep intact (recent context).
pub(crate) const MIN_RECENT_MESSAGES: usize = 6;

/// Target compression ratio for summarized content.
const BASE_CHUNK_RATIO: f64 = 0.4;

/// Schema version for [`ToolResultPlaceholder`] persistence.
pub const TOOL_RESULT_PLACEHOLDER_SCHEMA_VERSION: u32 = 1;

/// Prefix stamped into tool-result content when a pruning pass replaces the
/// original with a typed [`ToolResultPlaceholder`]. Used by replay parsing to
/// recognise a placeholder without teaching every downstream pipeline about
/// the M6.3 shape.
pub const TOOL_RESULT_PLACEHOLDER_PREFIX: &str = "[OCTOS_TOOL_RESULT_PLACEHOLDER]";

const TOOL_RESULT_PLACEHOLDER_SCHEMA_V1: &str = "octos.tool_result_placeholder.v1";

/// Find the boundary between old (compactable) and recent (kept verbatim) messages.
///
/// Returns the index where the recent zone starts. Messages `[1..split]` are old,
/// `[split..]` are recent. Never splits inside an assistant-tool pair.
pub(crate) fn find_recent_boundary(messages: &[Message], budget: u32, system_tokens: u32) -> usize {
    let mut recent_tokens = 0u32;
    let mut count = 0usize;
    let mut split = messages.len();

    for i in (1..messages.len()).rev() {
        let msg_tokens = estimate_message_tokens(&messages[i]);
        count += 1;

        if count >= MIN_RECENT_MESSAGES && system_tokens + recent_tokens + msg_tokens > budget / 2 {
            break;
        }

        recent_tokens += msg_tokens;
        split = i;
    }

    // Don't split inside a tool-call group: if split points to a Tool message,
    // walk back past all consecutive Tool messages and the preceding Assistant
    // message (which may have multiple parallel tool_calls).
    while split > 1 && messages[split].role == MessageRole::Tool {
        split -= 1;
    }

    split
}

/// Build an extractive summary of old messages within a token budget.
///
/// Extracts first lines from each message, strips tool call arguments
/// (security: untrusted payloads), and drops media references.
pub fn compact_messages(messages: &[Message], budget_tokens: u32) -> String {
    let mut lines = Vec::new();
    let header = format!(
        "## Conversation Summary (compacted from {} messages)\n",
        messages.len()
    );
    let mut running_tokens = estimate_tokens(&header);
    lines.push(header);

    let target = (budget_tokens as f64 * BASE_CHUNK_RATIO) as u32;

    for (i, msg) in messages.iter().enumerate() {
        if running_tokens >= target {
            lines.push(format!(
                "... ({} earlier messages omitted)",
                messages.len() - i
            ));
            break;
        }

        let line = summarize_message(msg, messages);
        let line_tokens = estimate_tokens(&line);

        if running_tokens + line_tokens > budget_tokens {
            lines.push(format!(
                "... ({} earlier messages omitted)",
                messages.len() - i
            ));
            break;
        }

        running_tokens += line_tokens;
        lines.push(line);
    }

    lines.join("\n")
}

/// Summarize a single message into a compact text line.
fn summarize_message(msg: &Message, context: &[Message]) -> String {
    match msg.role {
        MessageRole::User => {
            let media_note = if msg.media.is_empty() {
                ""
            } else {
                " [media omitted]"
            };
            format!("> User: {}{}", first_line(&msg.content, 200), media_note)
        }
        MessageRole::Assistant => {
            let mut parts = Vec::new();
            if let Some(ref calls) = msg.tool_calls {
                for call in calls {
                    parts.push(format!("- Called {}", call.name));
                }
            }
            if !msg.content.is_empty() {
                let prefix = if msg.tool_calls.is_some() {
                    "  "
                } else {
                    "> Assistant: "
                };
                parts.push(format!("{}{}", prefix, first_line(&msg.content, 200)));
            }
            parts.join("\n")
        }
        MessageRole::Tool => {
            let tool_name = find_tool_name(msg, context);
            let status = if msg.content.starts_with("Error:") {
                "error"
            } else {
                "ok"
            };
            format!(
                "  -> {}: {} - {}",
                tool_name,
                status,
                first_line(&msg.content, 100)
            )
        }
        MessageRole::System => {
            format!("> Context: {}", first_line(&msg.content, 200))
        }
    }
}

/// Extract the first line of text, truncated to max_chars (UTF-8 safe).
fn first_line(s: &str, max_chars: usize) -> String {
    let line = s.lines().next().unwrap_or("");
    if line.chars().count() <= max_chars {
        line.to_string()
    } else {
        let end = line
            .char_indices()
            .nth(max_chars)
            .map(|(i, _)| i)
            .unwrap_or(line.len());
        format!("{}...", &line[..end])
    }
}

/// Resolve a tool message's tool_call_id to the tool name from context.
fn find_tool_name(tool_msg: &Message, messages: &[Message]) -> String {
    if let Some(ref target_id) = tool_msg.tool_call_id {
        for msg in messages.iter().rev() {
            if let Some(ref calls) = msg.tool_calls {
                for call in calls {
                    if call.id == *target_id {
                        return call.name.clone();
                    }
                }
            }
        }
    }
    "unknown_tool".to_string()
}

// ---------------------------------------------------------------------------
// M6.3 typed compaction API.
// ---------------------------------------------------------------------------

/// Re-export of the policy type for ergonomic call sites.
pub use crate::workspace_policy::CompactionPolicy as CompactionPolicyRef;

/// Declared artifact that must survive a compaction pass.
///
/// Carries the stable `name` (matches a key in `WorkspacePolicy.artifacts`) plus
/// the raw glob/path pattern declared there. The runner looks for occurrences
/// of `pattern` (or sensible prefixes) in the compacted message stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreservedArtifact {
    name: String,
    pattern: String,
}

impl PreservedArtifact {
    pub fn new(name: impl Into<String>, pattern: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            pattern: pattern.into(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn pattern(&self) -> &str {
        &self.pattern
    }
}

/// Phase name for compaction-phase events (kind=phase).
pub const COMPACTION_PHASE: &str = "compaction";

/// The specific stage of the compaction pipeline. Emitted on phase events so
/// operators can distinguish a preflight pass from a post-LLM compaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionPhase {
    /// Compaction fires before the first LLM call of a turn.
    Preflight,
    /// Compaction fires at the top of a loop iteration after the first.
    TurnEnd,
    /// On-demand compaction requested by a caller (e.g. tests).
    OnDemand,
}

impl CompactionPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Preflight => "preflight",
            Self::TurnEnd => "turn_end",
            Self::OnDemand => "on_demand",
        }
    }
}

/// Outcome of a single compaction pass.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompactionOutcome {
    /// Whether any compaction work actually took place.
    pub performed: bool,
    /// Number of old messages folded into the summary.
    pub messages_dropped: usize,
    /// Number of tool-results replaced with a typed placeholder.
    pub tool_results_replaced: usize,
    /// Approximate token estimate before compaction.
    pub tokens_before: u32,
    /// Approximate token estimate after compaction.
    pub tokens_after: u32,
    /// Which summarizer flavour handled the pass.
    pub summarizer_kind: &'static str,
}

/// Result of the preservation check — which declared artifacts/invariants were
/// dropped during compaction.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PreservationLedger {
    /// Declared artifacts that remain referenced in the compacted messages.
    pub preserved: Vec<PreservedArtifact>,
    /// Declared artifacts or invariants that are no longer referenced.
    pub missing: Vec<PreservedArtifact>,
}

impl PreservationLedger {
    pub fn all_preserved(&self) -> bool {
        self.missing.is_empty()
    }
}

/// Report from a tool-result pruning pass.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolPruneReport {
    /// Number of tool-result messages replaced with a typed placeholder.
    pub replaced: usize,
}

/// Typed placeholder persisted in place of a pruned tool result.
///
/// Survives JSON round-trip via [`to_placeholder_content`] /
/// [`from_placeholder_content`]. Prefixed with
/// [`TOOL_RESULT_PLACEHOLDER_PREFIX`] so the runtime can detect old
/// placeholders during replay without misidentifying legitimate tool output
/// that happens to parse as JSON.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResultPlaceholder {
    /// Schema version; matches [`TOOL_RESULT_PLACEHOLDER_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Tool name that originally produced the result.
    pub tool_name: String,
    /// Tool call ID referenced by the preceding assistant message.
    pub tool_call_id: String,
    /// Logical turn this tool call was invoked in (1-indexed user turn).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u32>,
    /// Byte length of the original tool output, preserved for diagnostics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_byte_len: Option<u64>,
    /// Free-form reason string (e.g. `"pruned_after_turns"`).
    pub reason: String,
}

#[derive(Debug)]
pub enum ToolResultPlaceholderError {
    NotAPlaceholder,
    InvalidJson(serde_json::Error),
    UnsupportedSchema(String),
}

impl std::fmt::Display for ToolResultPlaceholderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAPlaceholder => f.write_str("not a tool-result placeholder"),
            Self::InvalidJson(err) => write!(f, "invalid tool-result placeholder JSON: {err}"),
            Self::UnsupportedSchema(name) => {
                write!(f, "unsupported tool-result placeholder schema: {name}")
            }
        }
    }
}

impl std::error::Error for ToolResultPlaceholderError {}

impl ToolResultPlaceholder {
    /// Serialize into a marker-prefixed JSON string suitable for storage in a
    /// `Message.content` field.
    pub fn to_placeholder_content(&self) -> String {
        let envelope = serde_json::json!({
            "schema": TOOL_RESULT_PLACEHOLDER_SCHEMA_V1,
            "schema_version": self.schema_version,
            "tool_name": self.tool_name,
            "tool_call_id": self.tool_call_id,
            "turn_id": self.turn_id,
            "original_byte_len": self.original_byte_len,
            "reason": self.reason,
        });
        format!(
            "{}{}",
            TOOL_RESULT_PLACEHOLDER_PREFIX,
            serde_json::to_string(&envelope)
                .unwrap_or_else(|_| "{\"schema\":\"octos.tool_result_placeholder.v1\"}".into())
        )
    }

    /// Parse a placeholder back from message content. Returns
    /// [`ToolResultPlaceholderError::NotAPlaceholder`] when the content does
    /// not carry the prefix.
    pub fn from_placeholder_content(content: &str) -> Result<Self, ToolResultPlaceholderError> {
        let rest = content
            .strip_prefix(TOOL_RESULT_PLACEHOLDER_PREFIX)
            .ok_or(ToolResultPlaceholderError::NotAPlaceholder)?;
        let value: serde_json::Value =
            serde_json::from_str(rest).map_err(ToolResultPlaceholderError::InvalidJson)?;
        let schema = value
            .get("schema")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if schema != TOOL_RESULT_PLACEHOLDER_SCHEMA_V1 {
            return Err(ToolResultPlaceholderError::UnsupportedSchema(
                schema.to_string(),
            ));
        }
        let placeholder = serde_json::from_value::<ToolResultPlaceholder>(value)
            .map_err(ToolResultPlaceholderError::InvalidJson)?;
        if placeholder.schema_version > TOOL_RESULT_PLACEHOLDER_SCHEMA_VERSION {
            return Err(ToolResultPlaceholderError::UnsupportedSchema(format!(
                "v{}",
                placeholder.schema_version
            )));
        }
        Ok(placeholder)
    }
}

/// Declarative compaction runner.
pub struct CompactionRunner {
    policy: CompactionPolicy,
    summarizer: Arc<dyn Summarizer>,
    event_sink: Option<EventSink>,
    repo_label: Option<String>,
    artifacts: WorkspaceArtifactsPolicy,
    /// Overrides the preserved_artifacts patterns when the caller wires a
    /// workspace-policy-bound runner via `with_workspace_policy`.
    resolved_preserved: Vec<PreservedArtifact>,
}

struct EventSink {
    path: String,
    session_id: String,
    task_id: String,
}

impl std::fmt::Debug for CompactionRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompactionRunner")
            .field("policy", &self.policy)
            .field("summarizer", &self.summarizer.kind())
            .field("has_event_sink", &self.event_sink.is_some())
            .field("repo_label", &self.repo_label)
            .field("preserved", &self.resolved_preserved)
            .finish()
    }
}

impl CompactionRunner {
    /// Build a runner from a typed policy. Defaults the summarizer to the
    /// extractive fallback and leaves the event sink unset.
    pub fn new(policy: CompactionPolicy) -> Self {
        let summarizer: Arc<dyn Summarizer> = default_summarizer_for(policy.summarizer);
        Self {
            policy,
            summarizer,
            event_sink: None,
            repo_label: None,
            artifacts: WorkspaceArtifactsPolicy::default(),
            resolved_preserved: Vec::new(),
        }
    }

    /// Override the summarizer implementation (e.g. swap in the LLM-iterative
    /// variant in M6.4).
    pub fn with_summarizer<S: Summarizer + 'static>(mut self, summarizer: S) -> Self {
        self.summarizer = Arc::new(summarizer);
        self
    }

    /// Route `octos.harness.event.v1 { kind: phase }` events to `sink_path`
    /// for the given session/task IDs.
    pub fn with_event_sink(
        mut self,
        sink_path: impl Into<String>,
        session_id: impl Into<String>,
        task_id: impl Into<String>,
    ) -> Self {
        self.event_sink = Some(EventSink {
            path: sink_path.into(),
            session_id: session_id.into(),
            task_id: task_id.into(),
        });
        self
    }

    /// Attach a repository label used as the workflow tag on phase events.
    pub fn with_repo_label(mut self, label: impl Into<String>) -> Self {
        self.repo_label = Some(label.into());
        self
    }

    /// Resolve `preserved_artifacts` names against a [`WorkspacePolicy`] so the
    /// runner knows which raw path/glob patterns to look for in messages.
    pub fn with_workspace_policy(mut self, workspace: &WorkspacePolicy) -> Self {
        self.artifacts = workspace.artifacts.clone();
        self.resolved_preserved = self
            .policy
            .preserved_artifacts
            .iter()
            .map(|name| {
                let pattern = workspace
                    .artifacts
                    .entries
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| name.clone());
                PreservedArtifact::new(name.clone(), pattern)
            })
            .collect();
        self
    }

    /// Access the underlying policy.
    pub fn policy(&self) -> &CompactionPolicy {
        &self.policy
    }

    /// Access the summarizer kind for diagnostics (e.g. logs/metrics).
    pub fn summarizer_kind(&self) -> &'static str {
        self.summarizer.kind()
    }

    /// Decide whether preflight compaction should fire for `messages`.
    ///
    /// Returns `Some(estimated_tokens)` when the conversation exceeds the
    /// policy's `preflight_threshold`, and `None` otherwise (including when
    /// preflight is disabled).
    pub fn needs_preflight(&self, messages: &[Message]) -> Option<u32> {
        let threshold = self.policy.preflight_threshold?;
        let total: u32 = messages.iter().map(estimate_message_tokens).sum();
        if total > threshold { Some(total) } else { None }
    }

    /// Run a compaction pass in-place.
    ///
    /// Emits `octos.harness.event.v1 { kind: phase }` events for `start` and
    /// `complete` so operators can observe the policy in action. Compaction is
    /// idempotent: calling it a second time on the already-compacted history
    /// is a no-op when the conversation already fits under the token budget.
    pub fn run(&self, messages: &mut Vec<Message>, phase: CompactionPhase) -> CompactionOutcome {
        let tokens_before: u32 = messages.iter().map(estimate_message_tokens).sum();
        self.emit_phase_event(phase, "start", tokens_before);

        let prune = self.prune_tool_results(messages);

        let budget = self.policy.token_budget;
        let mut outcome = CompactionOutcome {
            performed: prune.replaced > 0,
            messages_dropped: 0,
            tool_results_replaced: prune.replaced,
            tokens_before,
            tokens_after: 0,
            summarizer_kind: self.summarizer.kind(),
        };

        if tokens_before <= budget {
            // Nothing to summarise — only the pruning step ran.
            let tokens_after: u32 = messages.iter().map(estimate_message_tokens).sum();
            outcome.tokens_after = tokens_after;
            self.emit_phase_event(phase, "complete", tokens_after);
            return outcome;
        }

        // Compute the recent boundary against the policy budget (not the
        // provider context window — the policy owns its own budget).
        let system_tokens = if messages.is_empty() {
            0
        } else {
            estimate_message_tokens(&messages[0])
        };
        if system_tokens >= budget {
            warn!(
                system_tokens,
                budget, "compaction: system prompt exceeds policy budget; skipping summary"
            );
            let tokens_after: u32 = messages.iter().map(estimate_message_tokens).sum();
            outcome.tokens_after = tokens_after;
            self.emit_phase_event(phase, "complete", tokens_after);
            return outcome;
        }

        if messages.len() < 2 {
            let tokens_after: u32 = messages.iter().map(estimate_message_tokens).sum();
            outcome.tokens_after = tokens_after;
            self.emit_phase_event(phase, "complete", tokens_after);
            return outcome;
        }

        let split = find_recent_boundary(messages, budget, system_tokens);
        if split <= 1 {
            // Too few messages for the recent-boundary heuristic, but we
            // still exceed the budget — fall back to oldest-first trim so
            // preflight actually makes progress.
            let dropped = fallback_trim(messages, budget);
            outcome.performed = outcome.performed || dropped > 0;
            outcome.messages_dropped = dropped;
            let tokens_after: u32 = messages.iter().map(estimate_message_tokens).sum();
            outcome.tokens_after = tokens_after;
            self.emit_phase_event(phase, "complete", tokens_after);
            return outcome;
        }

        let recent_tokens: u32 = messages[split..].iter().map(estimate_message_tokens).sum();
        if system_tokens + recent_tokens >= budget {
            // Recent+system already exceeds the budget; trim oldest messages
            // (excluding the system prompt) until we fit.
            let dropped = fallback_trim(messages, budget);
            outcome.performed = outcome.performed || dropped > 0;
            outcome.messages_dropped = dropped;
            let tokens_after: u32 = messages.iter().map(estimate_message_tokens).sum();
            outcome.tokens_after = tokens_after;
            self.emit_phase_event(phase, "complete", tokens_after);
            return outcome;
        }

        let summary_budget = budget.saturating_sub(system_tokens + recent_tokens);
        let old_messages: Vec<Message> = messages[1..split].to_vec();
        if old_messages.is_empty() {
            let tokens_after: u32 = messages.iter().map(estimate_message_tokens).sum();
            outcome.tokens_after = tokens_after;
            self.emit_phase_event(phase, "complete", tokens_after);
            return outcome;
        }

        let dropped = old_messages.len();
        let summary_text = match self.summarizer.summarize(&old_messages, summary_budget) {
            Ok(s) => s,
            Err(err) => {
                warn!(error = %err, "compaction: summarizer failed, falling back to extractive");
                compact_messages(&old_messages, summary_budget)
            }
        };

        messages.drain(1..split);
        messages.insert(
            1,
            Message {
                role: MessageRole::System,
                content: summary_text,
                media: vec![],
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                timestamp: Utc::now(),
            },
        );
        outcome.performed = true;
        outcome.messages_dropped = dropped;

        let tokens_after: u32 = messages.iter().map(estimate_message_tokens).sum();
        outcome.tokens_after = tokens_after;
        self.emit_phase_event(phase, "complete", tokens_after);
        outcome
    }

    /// Replace tool-result messages older than `prune_tool_results_after_turns`
    /// user-turn boundaries with a typed [`ToolResultPlaceholder`].
    pub fn prune_tool_results(&self, messages: &mut [Message]) -> ToolPruneReport {
        let Some(keep_turns) = self.policy.prune_tool_results_after_turns else {
            return ToolPruneReport::default();
        };
        if keep_turns == 0 {
            return ToolPruneReport::default();
        }

        // Collect indices of user messages — they define turn boundaries.
        let user_indices: Vec<usize> = messages
            .iter()
            .enumerate()
            .filter_map(|(i, m)| (m.role == MessageRole::User).then_some(i))
            .collect();
        if user_indices.is_empty() {
            return ToolPruneReport::default();
        }

        let total_turns = user_indices.len();
        if (keep_turns as usize) >= total_turns {
            return ToolPruneReport::default();
        }
        // First N-keep user indices are "old": anything at or before the last
        // old user message is pruneable.
        let old_cutoff = user_indices[total_turns.saturating_sub(keep_turns as usize)];

        let mut replaced = 0usize;
        // Build a map id -> (tool_name, turn_id) from assistant messages up
        // to the cutoff.
        let mut turn_counter: u32 = 0;
        let mut id_to_meta: std::collections::HashMap<String, (String, u32)> =
            std::collections::HashMap::new();
        for (idx, msg) in messages.iter().enumerate() {
            if msg.role == MessageRole::User {
                turn_counter += 1;
            }
            if idx > old_cutoff {
                break;
            }
            if msg.role == MessageRole::Assistant {
                if let Some(ref calls) = msg.tool_calls {
                    for call in calls {
                        id_to_meta
                            .entry(call.id.clone())
                            .or_insert_with(|| (call.name.clone(), turn_counter));
                    }
                }
            }
        }

        for (idx, msg) in messages.iter_mut().enumerate() {
            if idx > old_cutoff {
                break;
            }
            if msg.role != MessageRole::Tool {
                continue;
            }
            if ToolResultPlaceholder::from_placeholder_content(&msg.content).is_ok() {
                // Already pruned on an earlier pass.
                continue;
            }
            let tool_id = msg.tool_call_id.clone().unwrap_or_default();
            let (tool_name, turn_id) = id_to_meta
                .get(&tool_id)
                .cloned()
                .unwrap_or_else(|| ("unknown_tool".to_string(), 0));
            let placeholder = ToolResultPlaceholder {
                schema_version: TOOL_RESULT_PLACEHOLDER_SCHEMA_VERSION,
                tool_name,
                tool_call_id: tool_id,
                turn_id: Some(turn_id),
                original_byte_len: Some(msg.content.len() as u64),
                reason: "pruned_after_turns".to_string(),
            };
            msg.content = placeholder.to_placeholder_content();
            replaced += 1;
        }

        ToolPruneReport { replaced }
    }

    /// Check that every declared `preserved_artifact` and `preserved_invariant`
    /// is still referenced in the compacted message stream.
    pub fn check_preserved(
        &self,
        messages: &[Message],
        workspace: &WorkspacePolicy,
    ) -> eyre::Result<PreservationLedger> {
        let mut preserved = Vec::new();
        let mut missing = Vec::new();

        // Concatenate message text once for substring matching; cheaper than a
        // regex engine and matches how downstream renderers see the stream.
        let mut haystack = String::new();
        for msg in messages {
            haystack.push_str(&msg.content);
            haystack.push('\n');
        }

        let artifact_list: Vec<PreservedArtifact> = if !self.resolved_preserved.is_empty() {
            self.resolved_preserved.clone()
        } else {
            self.policy
                .preserved_artifacts
                .iter()
                .map(|name| {
                    let pattern = workspace
                        .artifacts
                        .entries
                        .get(name)
                        .cloned()
                        .unwrap_or_else(|| name.clone());
                    PreservedArtifact::new(name.clone(), pattern)
                })
                .collect()
        };

        for artifact in &artifact_list {
            if matches_artifact(&haystack, artifact) {
                preserved.push(artifact.clone());
            } else {
                missing.push(artifact.clone());
            }
        }

        for invariant in &self.policy.preserved_invariants {
            if haystack.contains(invariant) {
                preserved.push(PreservedArtifact::new(
                    format!("invariant:{invariant}"),
                    invariant.clone(),
                ));
            } else {
                missing.push(PreservedArtifact::new(
                    format!("invariant:{invariant}"),
                    invariant.clone(),
                ));
            }
        }

        Ok(PreservationLedger { preserved, missing })
    }

    fn emit_phase_event(&self, phase: CompactionPhase, stage: &str, tokens: u32) {
        let Some(sink) = self.event_sink.as_ref() else {
            return;
        };
        let message = format!(
            "compaction {} ({}; tokens={} summarizer={})",
            stage,
            phase.as_str(),
            tokens,
            self.summarizer.kind()
        );
        let event = HarnessEvent::phase_event(
            sink.session_id.clone(),
            sink.task_id.clone(),
            self.repo_label.clone(),
            COMPACTION_PHASE.to_string(),
            Some(message),
        );
        if let Err(err) = write_event_to_sink(&sink.path, &event) {
            warn!(path = %sink.path, error = %err, "compaction: failed to emit phase event");
        }
    }
}

fn default_summarizer_for(kind: CompactionSummarizerKind) -> Arc<dyn Summarizer> {
    // Delegate to the summarizer module so provider-aware wiring
    // (`default_summarizer_for_with_provider`) and the plain extractive
    // default live in one place.
    crate::summarizer::default_summarizer_for(kind)
}

fn matches_artifact(haystack: &str, artifact: &PreservedArtifact) -> bool {
    let pattern = artifact.pattern();
    if pattern.is_empty() {
        return haystack.contains(artifact.name());
    }
    // Glob-like prefix match — trim the wildcard suffix and look for the
    // literal prefix (plus, separately, the raw path). This matches how
    // downstream workflow messages usually cite artifacts (`output/deck.pptx`
    // or `output/slide-1.png` from the `output/**/slide-*.png` pattern).
    let literal_prefix = pattern.split(['*', '?']).next().unwrap_or("");
    if !literal_prefix.is_empty() {
        let stripped = literal_prefix.trim_end_matches('/');
        if !stripped.is_empty() && haystack.contains(stripped) {
            return true;
        }
    }
    haystack.contains(pattern)
}

fn fallback_trim(messages: &mut Vec<Message>, budget: u32) -> usize {
    if messages.is_empty() {
        return 0;
    }
    let system_tokens = estimate_message_tokens(&messages[0]);
    let mut kept = system_tokens;
    let mut keep_from = messages.len();
    for i in (1..messages.len()).rev() {
        let t = estimate_message_tokens(&messages[i]);
        if kept + t > budget {
            break;
        }
        kept += t;
        keep_from = i;
    }
    // Keep at least 2 non-system messages.
    let max_keep_from = messages.len().saturating_sub(2);
    if keep_from > max_keep_from {
        keep_from = max_keep_from;
    }
    while keep_from > 1 && messages[keep_from].role == MessageRole::Tool {
        keep_from -= 1;
    }
    if keep_from > 1 {
        let dropped = keep_from - 1;
        messages.drain(1..keep_from);
        dropped
    } else {
        0
    }
}

/// Convenience: resolve the declared `preserved_artifacts` from a workspace
/// policy into typed [`PreservedArtifact`] records, skipping unknown names.
pub fn resolve_preserved_artifacts(
    policy: &CompactionPolicy,
    artifacts: &WorkspaceArtifactsPolicy,
) -> Vec<PreservedArtifact> {
    policy
        .preserved_artifacts
        .iter()
        .map(|name| {
            let pattern = artifacts
                .entries
                .get(name)
                .cloned()
                .unwrap_or_else(|| name.clone());
            PreservedArtifact::new(name.clone(), pattern)
        })
        .collect()
}

/// Drop-in helper for metrics: reports the current schema version number for
/// a policy file, or [`COMPACTION_POLICY_SCHEMA_VERSION`] when absent.
pub fn policy_schema_version(policy: Option<&CompactionPolicy>) -> u32 {
    policy
        .map(|p| p.schema_version)
        .unwrap_or(COMPACTION_POLICY_SCHEMA_VERSION)
}

/// Attempt to infer a repo label suitable for phase events from the workspace
/// root path.
pub fn repo_label_from_path(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_core::ToolCall;

    fn user_msg(content: &str) -> Message {
        Message {
            role: MessageRole::User,
            content: content.to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            timestamp: chrono::Utc::now(),
        }
    }

    fn assistant_msg(content: &str) -> Message {
        Message {
            role: MessageRole::Assistant,
            content: content.to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            timestamp: chrono::Utc::now(),
        }
    }

    fn assistant_tool_call(tool_name: &str, tool_id: &str) -> Message {
        Message {
            role: MessageRole::Assistant,
            content: String::new(),
            media: vec![],
            tool_calls: Some(vec![ToolCall {
                id: tool_id.to_string(),
                name: tool_name.to_string(),
                arguments: serde_json::json!({"path": "/secret/file", "content": "x".repeat(1000)}),
                metadata: None,
            }]),
            tool_call_id: None,
            reasoning_content: None,
            timestamp: chrono::Utc::now(),
        }
    }

    fn tool_result(tool_id: &str, content: &str) -> Message {
        Message {
            role: MessageRole::Tool,
            content: content.to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: Some(tool_id.to_string()),
            reasoning_content: None,
            timestamp: chrono::Utc::now(),
        }
    }

    fn system_msg(content: &str) -> Message {
        Message {
            role: MessageRole::System,
            content: content.to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            timestamp: chrono::Utc::now(),
        }
    }

    #[test]
    fn test_compact_messages_basic() {
        let messages = vec![
            user_msg("Hello, can you help me?"),
            assistant_msg("Sure, I can help!"),
            user_msg("Read the file"),
            assistant_tool_call("read_file", "tc1"),
            tool_result("tc1", "fn main() { println!(\"hello\"); }"),
            assistant_msg("Here is the file content."),
        ];

        let summary = compact_messages(&messages, 10000);
        assert!(summary.contains("Conversation Summary"));
        assert!(summary.contains("> User: Hello"));
        assert!(summary.contains("> Assistant: Sure"));
        assert!(summary.contains("Called read_file"));
        assert!(summary.contains("-> read_file: ok"));
    }

    #[test]
    fn test_compact_strips_tool_arguments() {
        let messages = vec![
            assistant_tool_call("write_file", "tc1"),
            tool_result("tc1", "File written."),
        ];

        let summary = compact_messages(&messages, 10000);
        assert!(summary.contains("Called write_file"));
        assert!(!summary.contains("/secret/file"));
        assert!(!summary.contains("xxxx"));
    }

    #[test]
    fn test_compact_budget_enforcement() {
        let mut messages = Vec::new();
        for i in 0..50 {
            messages.push(user_msg(&format!("Message number {} with some content", i)));
            messages.push(assistant_msg(&format!("Response number {} here", i)));
        }

        let summary = compact_messages(&messages, 200);
        let summary_tokens = estimate_tokens(&summary);
        assert!(summary_tokens <= 200);
        assert!(summary.contains("earlier messages omitted"));
    }

    #[test]
    fn test_compact_media_omitted() {
        let messages = vec![Message {
            role: MessageRole::User,
            content: "Look at this image".to_string(),
            media: vec!["photo.jpg".to_string()],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            timestamp: chrono::Utc::now(),
        }];

        let summary = compact_messages(&messages, 10000);
        assert!(summary.contains("[media omitted]"));
        assert!(!summary.contains("photo.jpg"));
    }

    #[test]
    fn test_compact_error_tool_result() {
        let messages = vec![
            assistant_tool_call("shell", "tc1"),
            tool_result("tc1", "Error: command not found"),
        ];

        let summary = compact_messages(&messages, 10000);
        assert!(summary.contains("-> shell: error"));
    }

    #[test]
    fn test_find_recent_boundary_tool_pairing() {
        let mut messages = vec![system_msg("system prompt")];
        for i in 0..5 {
            messages.push(user_msg(&format!(
                "question {} with enough text to use tokens",
                i
            )));
            messages.push(assistant_msg(&format!(
                "answer {} with enough text to use tokens",
                i
            )));
        }
        messages.push(assistant_tool_call("read_file", "tc1"));
        messages.push(tool_result("tc1", "file content here"));
        for i in 5..10 {
            messages.push(user_msg(&format!(
                "question {} with enough text to use tokens",
                i
            )));
            messages.push(assistant_msg(&format!(
                "answer {} with enough text to use tokens",
                i
            )));
        }

        let split = find_recent_boundary(&messages, 200, 50);
        assert!(split > 1, "budget should force compaction, split={split}");
        assert_ne!(messages[split].role, MessageRole::Tool);
    }

    #[test]
    fn test_first_line_utf8_safe() {
        let text = "Hello world";
        assert_eq!(first_line(text, 5), "Hello...");

        let cjk = "你好世界测试文本";
        assert_eq!(first_line(cjk, 4), "你好世界...");

        let short = "hi";
        assert_eq!(first_line(short, 100), "hi");
    }

    #[test]
    fn test_find_tool_name_resolves() {
        let messages = vec![
            assistant_tool_call("grep", "tc1"),
            tool_result("tc1", "found matches"),
        ];
        let name = find_tool_name(&messages[1], &messages);
        assert_eq!(name, "grep");
    }

    #[test]
    fn test_find_tool_name_unknown_fallback() {
        let msg = tool_result("nonexistent", "data");
        let name = find_tool_name(&msg, &[]);
        assert_eq!(name, "unknown_tool");
    }

    #[test]
    fn test_summarize_user_message() {
        let msg = user_msg("Hello world");
        let summary = summarize_message(&msg, &[]);
        assert_eq!(summary, "> User: Hello world");
    }

    #[test]
    fn test_summarize_user_message_with_media() {
        let msg = Message {
            role: MessageRole::User,
            content: "Check this".to_string(),
            media: vec!["img.png".to_string()],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            timestamp: chrono::Utc::now(),
        };
        let summary = summarize_message(&msg, &[]);
        assert!(summary.contains("[media omitted]"));
        assert!(summary.contains("Check this"));
    }

    #[test]
    fn test_summarize_assistant_text() {
        let msg = assistant_msg("Here is your answer");
        let summary = summarize_message(&msg, &[]);
        assert_eq!(summary, "> Assistant: Here is your answer");
    }

    #[test]
    fn test_summarize_assistant_tool_call() {
        let msg = assistant_tool_call("read_file", "tc1");
        let summary = summarize_message(&msg, &[]);
        assert!(summary.contains("Called read_file"));
    }

    #[test]
    fn test_summarize_tool_result_ok() {
        let context = vec![assistant_tool_call("grep", "tc1")];
        let msg = tool_result("tc1", "found 3 matches");
        let summary = summarize_message(&msg, &context);
        assert!(summary.contains("-> grep: ok"));
    }

    #[test]
    fn test_summarize_tool_result_error() {
        let context = vec![assistant_tool_call("shell", "tc1")];
        let msg = tool_result("tc1", "Error: command not found");
        let summary = summarize_message(&msg, &context);
        assert!(summary.contains("-> shell: error"));
    }

    #[test]
    fn test_summarize_system_message() {
        let msg = system_msg("You are a coding assistant");
        let summary = summarize_message(&msg, &[]);
        assert_eq!(summary, "> Context: You are a coding assistant");
    }

    #[test]
    fn test_first_line_multiline() {
        let text = "first line\nsecond line\nthird line";
        assert_eq!(first_line(text, 200), "first line");
    }

    #[test]
    fn test_first_line_empty() {
        assert_eq!(first_line("", 200), "");
    }

    #[test]
    fn tool_result_placeholder_roundtrips() {
        let p = ToolResultPlaceholder {
            schema_version: TOOL_RESULT_PLACEHOLDER_SCHEMA_VERSION,
            tool_name: "shell".into(),
            tool_call_id: "id1".into(),
            turn_id: Some(2),
            original_byte_len: Some(1234),
            reason: "pruned_after_turns".into(),
        };
        let content = p.to_placeholder_content();
        assert!(content.starts_with(TOOL_RESULT_PLACEHOLDER_PREFIX));
        let parsed = ToolResultPlaceholder::from_placeholder_content(&content).unwrap();
        assert_eq!(parsed, p);
    }

    #[test]
    fn tool_result_placeholder_rejects_non_prefix() {
        let err = ToolResultPlaceholder::from_placeholder_content("plain text").unwrap_err();
        assert!(matches!(err, ToolResultPlaceholderError::NotAPlaceholder));
    }

    #[test]
    fn runner_respects_default_policy_absence_invariant() {
        let policy = CompactionPolicy {
            token_budget: 10_000,
            ..Default::default()
        };
        let runner = CompactionRunner::new(policy);
        let mut messages = vec![user_msg("hi")];
        let outcome = runner.run(&mut messages, CompactionPhase::OnDemand);
        assert!(!outcome.performed);
    }

    #[test]
    fn runner_preflight_threshold_detects_overflow() {
        let policy = CompactionPolicy {
            token_budget: 10_000,
            preflight_threshold: Some(10),
            ..Default::default()
        };
        let runner = CompactionRunner::new(policy);
        let messages = vec![user_msg(&"x".repeat(500))];
        assert!(runner.needs_preflight(&messages).is_some());
    }

    #[test]
    fn runner_prune_tool_results_skips_when_disabled() {
        let policy = CompactionPolicy {
            prune_tool_results_after_turns: None,
            ..Default::default()
        };
        let runner = CompactionRunner::new(policy);
        let mut messages = vec![
            user_msg("question"),
            assistant_tool_call("shell", "tc1"),
            tool_result("tc1", "big"),
        ];
        let report = runner.prune_tool_results(&mut messages);
        assert_eq!(report.replaced, 0);
    }

    #[test]
    fn matches_artifact_supports_glob_prefix() {
        let art = PreservedArtifact::new("deck", "output/**/slide-*.png");
        assert!(matches_artifact(
            "rendered output/sub/slide-1.png successfully",
            &art
        ));
        let art2 = PreservedArtifact::new("primary", "output/deck.pptx");
        assert!(matches_artifact("wrote to output/deck.pptx earlier", &art2));
        let art3 = PreservedArtifact::new("other", "never/mentioned.txt");
        assert!(!matches_artifact("no mention here", &art3));
    }
}
