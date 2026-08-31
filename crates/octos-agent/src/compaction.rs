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
use std::time::Duration;

use chrono::Utc;
use octos_core::{Message, MessageRole};
use octos_llm::ChatConfig;
use octos_llm::LlmProvider;
use octos_llm::context::{estimate_message_tokens, estimate_tokens};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::abi_schema::COMPACTION_POLICY_SCHEMA_VERSION;
use crate::harness_events::{HarnessEvent, write_event_to_sink};
use crate::summarizer::default_summarizer_for_with_provider;
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
    // #2132: the plan block is carved OUT of the summary budget, not stacked
    // on top of it — preservation must not push the artifact past the size
    // the caller's threshold math assumed. Living inside the producer (this
    // function and llm_compaction_summary) means every compaction path —
    // AppUI, session actor, legacy agent channel, summarizer tiers —
    // inherits preservation without per-site wiring.
    let plan = latest_plan_snapshot(messages);
    let plan_budget_tokens = plan
        .as_deref()
        .map(|p| estimate_tokens(p).saturating_add(24))
        .unwrap_or(0);
    let budget_tokens = budget_tokens.saturating_sub(plan_budget_tokens).max(64);

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

    prepend_plan_block(
        lines.join("\n"),
        plan,
        (plan_budget_tokens as usize).saturating_mul(4),
    )
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
    /// The summary text folded into the compacted prompt, when a pass
    /// produced one. Exposed so a conversational loop can persist it as a
    /// searchable episode (#1587 write side).
    pub summary: Option<String>,
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
            // #2131: the placeholder already carries `tool_call_id`, and the
            // `recall` tool's description tells the model to restore an evicted
            // output by exactly that id — so no in-placeholder call hint is
            // needed. Emitting one here would also mislead the chat/acp/mcp
            // paths, which build placeholders but register no recall tool.
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

    /// Build a runner from a typed policy, enabling the LLM-iterative
    /// summarizer when the policy declares
    /// [`CompactionSummarizerKind::LlmIterative`].
    ///
    /// This is the wiring seam used by `octos-cli` when a workspace policy
    /// requests the M6.4 iterative flavour: the caller hands in the agent's
    /// [`LlmProvider`] and this constructor selects the matching summarizer
    /// via [`default_summarizer_for_with_provider`]. Extractive policies
    /// behave identically to [`Self::new`].
    pub fn with_provider(policy: CompactionPolicy, provider: Arc<dyn LlmProvider>) -> Self {
        let summarizer: Arc<dyn Summarizer> =
            default_summarizer_for_with_provider(policy.summarizer, Some(provider));
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
            summary: None,
        };

        // Budget decision uses the POST-prune estimate: when placeholder
        // pruning alone brought the conversation under budget, summarizing
        // old messages would discard context for no budget reason.
        // `tokens_before` (pre-prune) is preserved on the outcome for
        // reporting.
        let tokens_after_prune: u32 = messages.iter().map(estimate_message_tokens).sum();
        if tokens_after_prune <= budget {
            // Nothing to summarise — only the pruning step ran.
            outcome.tokens_after = tokens_after_prune;
            self.emit_phase_event(phase, "complete", tokens_after_prune);
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

        outcome.summary = Some(summary_text.clone());
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
                client_message_id: None,
                thread_id: None,
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

/// Byte cap on the preserved-plan block (bytes, matching
/// `octos_core::truncate_utf8` semantics): the plan is a checklist, not a
/// transcript — anything longer is a model mis-using update_plan.
const PLAN_SNAPSHOT_MAX_BYTES: usize = 1500;
/// Byte cap on a single checklist title — argument-derived text is
/// untrusted, so it is bounded and control-stripped, never free-form.
const PLAN_TITLE_MAX_BYTES: usize = 120;

/// Sentinels delimiting the preserved-plan block inside a compaction
/// summary. STATE, not instructions: the wording defers to the newest user
/// message on purpose — `specs/task-compaction-instruction-priority.spec.md`
/// exists because imperatives inside summaries were followed over fresh
/// user input, and an earlier cut of this feature ("resume from the first
/// unchecked item") reproduced exactly that bug. The BEGIN sentinel doubles
/// as the carry-forward marker: pass N+1 re-extracts the block from pass
/// N's summary text, so the plan survives ANY number of passes, not one.
pub(crate) const PLAN_BLOCK_BEGIN: &str = "## Task plan as last declared (background state — the newest user message decides what happens next)";
pub(crate) const PLAN_BLOCK_END: &str = "(end of plan state)";

/// Render `update_plan` arguments as a checklist, `None` for degenerate
/// input (empty plan, unparseable args, no recognizable titles) — callers
/// keep scanning older state rather than letting garbage shadow a valid
/// plan. Parsing is DELEGATED to the tool's own `normalize_plan` (one
/// parser, one set of status spellings); statuses render from the typed
/// enum. There is deliberately no raw-JSON fallback: tool arguments are
/// untrusted (compact_messages strips them for that reason), so only the
/// bounded, control-stripped checklist form ever enters a summary.
fn render_plan_checklist(args: &serde_json::Value) -> Option<String> {
    // #1711: models sometimes deliver arguments as a stringified object;
    // recover it the way the provider layer does.
    let parsed;
    let args = match args {
        serde_json::Value::String(raw) => {
            parsed = serde_json::from_str::<serde_json::Value>(raw).ok()?;
            &parsed
        }
        other => other,
    };
    let record = crate::tools::coding_tools::normalize_plan(args, 0);
    let mut body = String::new();
    for item in &record.items {
        let mut title: String = item
            .title
            .trim()
            .chars()
            .filter(|c| !c.is_control())
            .collect();
        if title.is_empty() {
            continue;
        }
        octos_core::truncate_utf8(&mut title, PLAN_TITLE_MAX_BYTES, "…");
        let marker = match item.status {
            octos_core::ui_protocol::PlanItemStatus::Completed => "[x]",
            octos_core::ui_protocol::PlanItemStatus::InProgress => "[>]",
            octos_core::ui_protocol::PlanItemStatus::Pending => "[ ]",
        };
        body.push_str("- ");
        body.push_str(marker);
        body.push(' ');
        body.push_str(&title);
        body.push('\n');
    }
    let body = body.trim_end().to_string();
    (!body.is_empty()).then_some(body)
}

/// The plan body carried inside a previously produced summary, if any —
/// the carry-forward source that makes preservation multi-pass.
fn extract_plan_block(text: &str) -> Option<String> {
    let start = text.find(PLAN_BLOCK_BEGIN)?;
    let after = &text[start + PLAN_BLOCK_BEGIN.len()..];
    let end = after.find(PLAN_BLOCK_END)?;
    let body = after[..end].trim();
    (!body.is_empty()).then(|| body.to_string())
}

/// Remove every plan block from produced summary text. An LLM summarizer
/// prompted for a faithful handoff will happily copy the previous pass's
/// block into its output; without stripping, each pass would stack one more
/// stale checklist under the fresh one.
fn strip_plan_blocks(summary: &str) -> String {
    let mut out = summary.to_string();
    while let (Some(start), Some(end_rel)) = (
        out.find(PLAN_BLOCK_BEGIN),
        out.find(PLAN_BLOCK_BEGIN)
            .and_then(|s| out[s..].find(PLAN_BLOCK_END).map(|e| s + e)),
    ) {
        let end = end_rel + PLAN_BLOCK_END.len();
        out.replace_range(start..end, "");
    }
    // A summarized prior summary can carry a DANGLING sentinel line (its
    // body was cut by line-level truncation, so the span loop above never
    // matches); drop any line still holding a sentinel so exactly one
    // fresh block exists after prepending.
    if out.contains(PLAN_BLOCK_BEGIN) || out.contains(PLAN_BLOCK_END) {
        out = out
            .lines()
            .filter(|line| !line.contains(PLAN_BLOCK_BEGIN) && !line.contains(PLAN_BLOCK_END))
            .collect::<Vec<_>>()
            .join("\n");
    }
    out.trim_start().to_string()
}

/// The newest plan state in `messages`, rendered as a checklist — or `None`
/// when the conversation never declared one.
///
/// Compaction destroys transcript state, and the plan IS transcript state:
/// the observed failure was a long task whose model, after compaction, no
/// longer knew what it was doing and fell back to summarizing the repo.
/// ONE reverse scan covers both sources in newest-first order: a live
/// `update_plan` tool call wins over an older summary's carried block, and
/// after a pass that dropped the tool-call rows, the carried block is what
/// survives. Degenerate calls (cleared plans, unparseable args) are
/// SKIPPED, not allowed to shadow an older valid plan.
pub fn latest_plan_snapshot(messages: &[Message]) -> Option<String> {
    for message in messages.iter().rev() {
        if let Some(calls) = message.tool_calls.as_ref() {
            for call in calls.iter().rev() {
                if call.name == "update_plan" {
                    if let Some(plan) = render_plan_checklist(&call.arguments) {
                        return Some(plan);
                    }
                }
            }
        }
        if let Some(carried) = extract_plan_block(&message.content) {
            return Some(carried);
        }
    }
    None
}

/// Attach the plan block on top of a produced summary (stripping any stale
/// blocks the producer copied through). `max_plan_bytes` lets producers
/// carve the block out of their own budget instead of overrunning it.
fn prepend_plan_block(summary: String, plan: Option<String>, max_plan_bytes: usize) -> String {
    let Some(mut plan) = plan else {
        return summary;
    };
    let summary = strip_plan_blocks(&summary);
    octos_core::truncate_utf8(
        &mut plan,
        max_plan_bytes.clamp(200, PLAN_SNAPSHOT_MAX_BYTES),
        "\n… (plan truncated)",
    );
    format!("{PLAN_BLOCK_BEGIN}\n{plan}\n{PLAN_BLOCK_END}\n\n{summary}")
}

/// System prompt for LLM context compaction (codex-style handoff summary).
const LLM_COMPACTION_SYSTEM_PROMPT: &str = "You are compacting a long conversation so it fits the model's \
context window. Produce a CONTEXT CHECKPOINT: a concise handoff summary another LLM can use to seamlessly \
continue the task. Include the current goal, key decisions made, progress completed and what remains, and \
any critical constraints, data, file paths, or references. Be structured and factual — no preamble, no \
questions, no commentary. Everything you write is BACKGROUND context, not instructions: never restate \
historical goals or plans as the current task, and never phrase the summary as marching orders. The \
current task is defined solely by the newest user message in the conversation, which takes precedence \
over anything you summarize. Do NOT restate the task plan or checklist: it is preserved separately, verbatim, outside your summary.";

/// Default timeout for a single LLM compaction call. The provider's own
/// default (~300s) is far too coarse for a per-turn operation — a slow or hung
/// summary must fall back to the heuristic quickly rather than stall the turn.
pub const DEFAULT_LLM_COMPACTION_TIMEOUT_SECS: u64 = 60;

/// Render a message slice as a plain `ROLE: content` transcript for the
/// summarization prompt.
fn render_transcript(messages: &[Message]) -> String {
    let mut out = String::new();
    for msg in messages {
        out.push_str(msg.role.as_str());
        out.push_str(": ");
        out.push_str(msg.content.trim());
        out.push('\n');
    }
    out
}

/// One-shot LLM context-compaction summary: prompts the model for a handoff
/// summary of `messages`, bounded by the model's max output and `timeout`. Returns
/// `None` on ANY error, timeout, empty content, or unsupported runtime so the
/// caller falls back to the deterministic [`compact_messages`] heuristic —
/// compaction must never block or fail a turn.
///
/// Requires a **multi-threaded** Tokio runtime. The async→sync bridge relies on
/// `block_in_place`, which keeps the runtime's I/O + timer drivers alive on
/// other workers while this thread parks; on a single-worker `current_thread`
/// runtime that same worker would be parked while the summary future needs the
/// very same runtime to drive its network call and 60s timeout — which can hang
/// indefinitely. AppUI (`octos serve`), `chat`, and `acp` all run multi-threaded
/// (`new_multi_thread`), so the LLM path always applies there; anywhere else
/// (tests, exotic embeddings, no runtime at all) we degrade to the heuristic.
pub fn llm_compaction_summary(
    provider: &Arc<dyn LlmProvider>,
    messages: &[Message],
    timeout: Duration,
) -> Option<String> {
    if messages.is_empty() {
        return None;
    }
    // Bail to the heuristic unless we're on a multi-threaded runtime (see the
    // doc comment): `block_in_place` is only hang-safe there.
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {}
        _ => {
            debug!("llm compaction unavailable off a multi-threaded runtime; using heuristic");
            return None;
        }
    }
    let provider = Arc::clone(provider);
    // #2132: preservation lives in the producer — compute the plan from the
    // same messages the summary covers, attach it to whatever the LLM
    // returns (stripping any stale block the LLM copied through).
    let plan = latest_plan_snapshot(messages);
    let transcript = render_transcript(messages);
    crate::summarizer::run_llm_call_blocking(async move {
        // Give the call the model's FULL output budget, not a small
        // summary-size heuristic. Reasoning models spend tokens on
        // `reasoning_content` before emitting the summary `content`; capping at
        // a small budget starves the summary and returns empty content (a
        // silent heuristic fallback). Mirrors codex, which sets no output cap on
        // its compaction turn — the system prompt keeps the summary concise.
        let config = ChatConfig {
            max_tokens: Some(provider.max_output_tokens()),
            // Low but non-zero: a factual handoff summary, lightly deterministic.
            temperature: Some(0.2),
            ..Default::default()
        };
        let request = vec![
            Message::system(LLM_COMPACTION_SYSTEM_PROMPT),
            Message::user(transcript),
        ];
        match tokio::time::timeout(timeout, provider.chat(&request, &[], &config)).await {
            Ok(Ok(response)) => response
                .content
                .map(|content| content.trim().to_string())
                .filter(|content| !content.is_empty())
                .map(|content| prepend_plan_block(content, plan, PLAN_SNAPSHOT_MAX_BYTES)),
            Ok(Err(error)) => {
                warn!(%error, "llm compaction summary failed; falling back to heuristic");
                None
            }
            Err(_) => {
                warn!(
                    timeout_secs = timeout.as_secs(),
                    "llm compaction summary timed out; falling back to heuristic"
                );
                None
            }
        }
    })
}

#[cfg(test)]
mod tests {

    #[test]
    fn llm_compaction_prompt_demotes_history_to_background() {
        // Spec task-compaction-instruction-priority: the summarizer must be
        // told its output is background, or it restates stale plans as the
        // "current goal" and the post-compaction model follows them instead
        // of the newest user instruction (live drift 2026-08-02).
        assert!(
            super::LLM_COMPACTION_SYSTEM_PROMPT.contains("BACKGROUND"),
            "prompt must demote summarized history to background"
        );
        assert!(
            super::LLM_COMPACTION_SYSTEM_PROMPT.contains("newest user message"),
            "prompt must anchor the current task to the newest user message"
        );
    }
    use super::*;
    use octos_core::ToolCall;
    use std::time::Duration;

    struct CompactionMockProvider {
        result: std::result::Result<String, String>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for CompactionMockProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &ChatConfig,
        ) -> eyre::Result<octos_llm::ChatResponse> {
            match &self.result {
                Ok(content) => Ok(octos_llm::ChatResponse {
                    content: Some(content.clone()),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    stop_reason: octos_llm::StopReason::EndTurn,
                    usage: octos_llm::TokenUsage::default(),
                    provider_index: None,
                }),
                Err(message) => Err(eyre::eyre!("{message}")),
            }
        }

        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &ChatConfig,
        ) -> eyre::Result<octos_llm::ChatStream> {
            unimplemented!("mock does not stream")
        }

        fn model_id(&self) -> &str {
            "mock-compaction"
        }

        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn llm_compaction_summary_returns_model_output() {
        let provider: Arc<dyn LlmProvider> = Arc::new(CompactionMockProvider {
            result: Ok("Goal: X. Done: Y. Next: Z.".into()),
        });
        let messages = vec![Message::user("do X"), Message::assistant("did Y")];
        let out = llm_compaction_summary(&provider, &messages, Duration::from_secs(5));
        assert_eq!(out.as_deref(), Some("Goal: X. Done: Y. Next: Z."));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn llm_compaction_summary_returns_none_on_provider_error() {
        // The caller falls back to the heuristic on None — a failing provider
        // must NEVER break or block the turn.
        let provider: Arc<dyn LlmProvider> = Arc::new(CompactionMockProvider {
            result: Err("provider down".into()),
        });
        let messages = vec![Message::user("do X")];
        let out = llm_compaction_summary(&provider, &messages, Duration::from_secs(5));
        assert!(out.is_none());
    }

    #[test]
    fn llm_compaction_summary_none_for_empty_messages() {
        // Short-circuits before any blocking call, so needs no runtime.
        let provider: Arc<dyn LlmProvider> = Arc::new(CompactionMockProvider {
            result: Ok("unused".into()),
        });
        assert!(llm_compaction_summary(&provider, &[], Duration::from_secs(5)).is_none());
    }

    #[tokio::test] // current_thread runtime (the default, no `flavor`)
    async fn llm_compaction_summary_degrades_to_heuristic_on_current_thread() {
        // The async→sync bridge is only hang-safe on a multi-threaded runtime,
        // so on a current_thread runtime the LLM path must be skipped entirely
        // (caller falls back to the heuristic) rather than risk a hang.
        let provider: Arc<dyn LlmProvider> = Arc::new(CompactionMockProvider {
            result: Ok("should not be used on current_thread".into()),
        });
        let messages = vec![Message::user("do X")];
        let out = llm_compaction_summary(&provider, &messages, Duration::from_secs(5));
        assert!(
            out.is_none(),
            "current_thread runtime must degrade to the heuristic, not run the LLM path"
        );
    }

    fn user_msg(content: &str) -> Message {
        Message {
            role: MessageRole::User,
            content: content.to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
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
            client_message_id: None,
            thread_id: None,
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
            client_message_id: None,
            thread_id: None,
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
            client_message_id: None,
            thread_id: None,
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
            client_message_id: None,
            thread_id: None,
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
            messages.push(user_msg(&format!("Message number {i} with some content")));
            messages.push(assistant_msg(&format!("Response number {i} here")));
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
            client_message_id: None,
            thread_id: None,
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
                "question {i} with enough text to use tokens"
            )));
            messages.push(assistant_msg(&format!(
                "answer {i} with enough text to use tokens"
            )));
        }
        messages.push(assistant_tool_call("read_file", "tc1"));
        messages.push(tool_result("tc1", "file content here"));
        for i in 5..10 {
            messages.push(user_msg(&format!(
                "question {i} with enough text to use tokens"
            )));
            messages.push(assistant_msg(&format!(
                "answer {i} with enough text to use tokens"
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
            client_message_id: None,
            thread_id: None,
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
        // The placeholder carries tool_call_id (the recall handle).
        assert!(content.contains("id1"), "{content}");
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

    /// When the tool-result pruning pass alone brings the conversation under
    /// the token budget, the runner must not go on to summarize/drop old
    /// messages based on the stale pre-prune estimate — that discards
    /// conversational context for no budget reason.
    #[test]
    fn runner_skips_summary_when_prune_brings_under_budget() {
        let policy = CompactionPolicy {
            token_budget: 1_000,
            prune_tool_results_after_turns: Some(1),
            ..Default::default()
        };
        let runner = CompactionRunner::new(policy);
        let mut messages = vec![system_msg("sys prompt")];
        // Turn 1 carries a huge, stale tool result (pruned to a placeholder).
        messages.push(user_msg("old question"));
        messages.push(assistant_tool_call("shell", "tc_old"));
        messages.push(tool_result("tc_old", &"x".repeat(8_000)));
        // 6 more modest turns so the post-prune total sits between
        // budget/2 and budget (the recent-boundary walk engages, so a
        // stale over-budget decision WOULD summarize old turns).
        for index in 0..6 {
            messages.push(user_msg(&format!(
                "question {index} {}",
                "detail ".repeat(30)
            )));
            messages.push(assistant_msg(&format!(
                "answer {index} {}",
                "reply ".repeat(30)
            )));
        }
        let message_count_before = messages.len();

        let outcome = runner.run(&mut messages, CompactionPhase::OnDemand);

        assert!(
            outcome.tool_results_replaced >= 1,
            "the stale tool result should be pruned"
        );
        assert_eq!(
            outcome.messages_dropped, 0,
            "pruning already satisfied the budget; no messages may be summarized away"
        );
        assert_eq!(
            messages.len(),
            message_count_before,
            "no summary row should be inserted and no messages drained"
        );
        assert!(
            !messages
                .iter()
                .any(|m| m.content.contains("Conversation Summary")),
            "no extractive summary should be generated"
        );
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

    /// #2132 helper: a message carrying an update_plan tool call.
    fn plan_message(steps: serde_json::Value) -> Message {
        use octos_core::ToolCall;
        let mut msg = Message::assistant("");
        msg.tool_calls = Some(vec![ToolCall {
            id: "c1".into(),
            name: "update_plan".into(),
            arguments: serde_json::json!({ "plan": steps }),
            metadata: None,
        }]);
        msg
    }

    /// #2132: the newest VALID plan wins, statuses render from the typed
    /// enum, and preservation happens inside the producer — compact_messages
    /// itself emits the block, so every compaction path inherits it.
    #[test]
    fn should_preserve_newest_plan_as_checklist_when_compacting() {
        let messages = vec![
            plan_message(serde_json::json!([{"step": "old step", "status": "pending"}])),
            Message::user("keep working"),
            plan_message(serde_json::json!([
                {"step": "convert dataloader", "status": "completed"},
                {"step": "convert attention", "status": "in_progress"},
                {"step": "port test harness", "status": "pending"}
            ])),
        ];
        let summary = compact_messages(&messages, 1024);
        assert!(summary.starts_with(PLAN_BLOCK_BEGIN), "{summary}");
        assert!(summary.contains("- [x] convert dataloader"), "{summary}");
        assert!(summary.contains("- [>] convert attention"), "{summary}");
        assert!(summary.contains("- [ ] port test harness"), "{summary}");
        assert!(!summary.contains("old step"), "newest plan wins: {summary}");
        assert!(summary.contains(PLAN_BLOCK_END), "{summary}");
        // Plan-free conversations carry no block.
        let plain = compact_messages(&[Message::user("hi")], 1024);
        assert!(!plain.contains(PLAN_BLOCK_BEGIN), "{plain}");
    }

    /// #2132 multi-pass: after pass 1 drops the tool-call rows, the block
    /// carried inside the prior summary text is re-extracted — the plan
    /// survives ANY number of passes, not one.
    #[test]
    fn should_carry_plan_forward_when_prior_summary_is_the_only_source() {
        let pass1 = compact_messages(
            &[plan_message(serde_json::json!([
                {"step": "convert attention", "status": "in_progress"}
            ]))],
            1024,
        );
        // Pass 2 input: only the prior summary text (as a user row) + chatter.
        let messages = vec![Message::user(pass1), Message::user("more work")];
        let snapshot = latest_plan_snapshot(&messages).expect("carried plan");
        assert!(snapshot.contains("- [>] convert attention"), "{snapshot}");
        let pass2 = compact_messages(&messages, 1024);
        assert!(pass2.starts_with(PLAN_BLOCK_BEGIN), "{pass2}");
        // Exactly ONE block: the carried copy inside the summarized prose
        // must not stack under the fresh one.
        assert_eq!(pass2.matches(PLAN_BLOCK_BEGIN).count(), 1, "{pass2}");
    }

    /// #2132 (#1711 shape): stringified-object arguments are recovered, and
    /// degenerate plans (cleared, unparseable) are SKIPPED so they cannot
    /// shadow an older valid plan. No raw-JSON fallback exists — tool
    /// arguments are untrusted.
    #[test]
    fn should_skip_degenerate_plans_and_recover_stringified_arguments() {
        use octos_core::ToolCall;
        let mut stringified = Message::assistant("");
        stringified.tool_calls = Some(vec![ToolCall {
            id: "c1".into(),
            name: "update_plan".into(),
            arguments: serde_json::Value::String(
                r#"{"plan":[{"step":"from stringified args","status":"pending"}]}"#.into(),
            ),
            metadata: None,
        }]);
        let snapshot = latest_plan_snapshot(&[stringified]).expect("recovered");
        assert!(
            snapshot.contains("- [ ] from stringified args"),
            "{snapshot}"
        );

        // A cleared plan (empty array) newest must NOT shadow the older
        // valid one, and alone must yield no block at all.
        let valid = plan_message(serde_json::json!([{"step": "real step", "status": "pending"}]));
        let cleared = plan_message(serde_json::json!([]));
        let snapshot = latest_plan_snapshot(&[valid, cleared.clone()]).expect("older valid plan");
        assert!(snapshot.contains("real step"), "{snapshot}");
        assert_eq!(latest_plan_snapshot(&[cleared]), None);
        // Unparseable/null arguments are equally inert.
        let mut null_args = Message::assistant("");
        null_args.tool_calls = Some(vec![ToolCall {
            id: "c1".into(),
            name: "update_plan".into(),
            arguments: serde_json::Value::Null,
            metadata: None,
        }]);
        assert_eq!(latest_plan_snapshot(&[null_args]), None);
    }

    /// #2132 budget: the block is carved out of the producer's own budget
    /// (a tiny budget still yields a bounded artifact), and oversized plans
    /// truncate with the marker.
    #[test]
    fn should_keep_combined_artifact_bounded_when_budget_is_small() {
        let steps: Vec<serde_json::Value> = (0..200)
            .map(|i| serde_json::json!({"step": format!("step number {i} with some length"), "status": "pending"}))
            .collect();
        let messages = vec![plan_message(serde_json::Value::Array(steps))];
        let summary = compact_messages(&messages, 256);
        assert!(summary.contains("(plan truncated)"), "{summary}");
        // Combined artifact stays in the same order of magnitude as the
        // budget (256 tokens ≈ 1KB) instead of stacking 1.5KB on top.
        assert!(
            summary.len() < 4096,
            "combined artifact must remain bounded, got {} bytes",
            summary.len()
        );
    }
}
