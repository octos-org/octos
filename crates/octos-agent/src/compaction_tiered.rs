//! Three-tier compaction surface (M8.5, issue #540).
//!
//! Today octos ships a single tier of compaction: the declarative
//! [`crate::compaction::CompactionRunner`] with contract-gated artifacts,
//! placeholder replacement, and token budgets.  Claude Code, by contrast,
//! runs three tiers and the cheap tier-1 pass alone keeps 20-40% of turns
//! from ever hitting the expensive summarizer.
//!
//! This module adds the first two tiers as independent policies and wraps
//! the existing runner behind a [`FullCompactor`] trait so the caller can
//! see a single [`TieredCompactionRunner`] surface:
//!
//! 1. [`MicroCompactionPolicy`] — per-iteration stale tool-result pruning.
//!    Cheap, synchronous, in-place.  Replaces oversized or stale tool
//!    results with a typed [`ToolResultPlaceholder`] so the `tool_call_id`
//!    (and therefore the assistant/tool pairing) stays intact.
//! 2. [`ApiMicroCompactionConfig`] — a *builder*, not a runtime loop.
//!    Emits the opaque `context_management` JSON payload that Anthropic's
//!    server-side `clear_tool_uses_20250919` mechanism expects.  The
//!    agent loop plumbs this into `ChatConfig.context_management` before
//!    every Anthropic request; other providers ignore it silently.
//! 3. [`FullCompactor`] — the existing heavy summary+contract-artifacts
//!    pass.  Unchanged; merely wrapped so the tiered runner can ask
//!    "should I run tier 3?" in one place.
//!
//! The runner is intentionally synchronous — callers that need async
//! summarisers can drive them from their own [`FullCompactor`] impl.

use octos_core::{Message, MessageRole};
use serde::{Deserialize, Serialize};

use crate::compaction::{
    CompactionOutcome, CompactionPhase, CompactionRunner as FullCompactionRunner,
    TOOL_RESULT_PLACEHOLDER_SCHEMA_VERSION, ToolResultPlaceholder,
};

// ─── Tier 1: MicroCompactionPolicy ───────────────────────────────────────────

/// Default age (in user turns) at which a tool result becomes pruneable.
pub const DEFAULT_TIER1_MAX_AGE_TURNS: u32 = 5;

/// Default byte threshold for immediate content-clearing (regardless of age).
pub const DEFAULT_TIER1_MAX_SIZE_BYTES_PER_RESULT: u32 = 8 * 1024;

/// Per-iteration stale tool-result pruning policy (tier 1).
///
/// Runs in-place over the conversation and replaces a tool result's content
/// with a typed [`ToolResultPlaceholder`] when either:
///
/// * the tool result is older than `max_age_turns` user-message boundaries, or
/// * the tool result's content is larger than `max_size_bytes_per_result`.
///
/// The `tool_call_id` is always preserved so the assistant/tool pairing the
/// provider enforces stays intact.  Tool results whose `tool_call_id` is
/// listed in `protected_tool_call_ids` are never touched, which lets the
/// caller hand off a set of IDs referenced by unresolved retry buckets or
/// workspace-contract artifacts.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MicroCompactionPolicy {
    /// Drop tool results older than this many user-turn boundaries.
    pub max_age_turns: u32,
    /// Tool results larger than this (in bytes) get content-cleared on sight.
    pub max_size_bytes_per_result: u32,
}

impl Default for MicroCompactionPolicy {
    fn default() -> Self {
        Self {
            max_age_turns: DEFAULT_TIER1_MAX_AGE_TURNS,
            max_size_bytes_per_result: DEFAULT_TIER1_MAX_SIZE_BYTES_PER_RESULT,
        }
    }
}

impl MicroCompactionPolicy {
    /// Convenience builder matching the parent module's fluent style.
    pub fn with_max_age_turns(mut self, max_age_turns: u32) -> Self {
        self.max_age_turns = max_age_turns;
        self
    }

    /// Convenience builder for the size threshold.
    pub fn with_max_size_bytes_per_result(mut self, max_size_bytes_per_result: u32) -> Self {
        self.max_size_bytes_per_result = max_size_bytes_per_result;
        self
    }

    /// Prune stale/oversized tool results in-place.
    ///
    /// `protected_tool_call_ids` receives tool_call IDs that must survive the
    /// pass untouched (e.g. those referenced by an unresolved retry bucket or
    /// by a contract-gated artifact awaiting delivery).
    pub fn prune(
        &self,
        messages: &mut [Message],
        protected_tool_call_ids: &[String],
    ) -> Tier1Report {
        if self.max_age_turns == 0 && self.max_size_bytes_per_result == u32::MAX {
            return Tier1Report::default();
        }

        // ID -> tool_name, turn_id so we can build a typed placeholder even
        // after the assistant message is far behind us.
        let mut id_to_meta: std::collections::HashMap<String, (String, u32)> =
            std::collections::HashMap::new();
        let mut turn_counter: u32 = 0;
        for msg in messages.iter() {
            if msg.role == MessageRole::User {
                turn_counter += 1;
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

        // Current turn = the highest user-turn index we counted.
        let current_turn = turn_counter;
        let age_threshold = self.max_age_turns;
        let size_threshold = self.max_size_bytes_per_result as usize;

        let mut results_pruned = 0usize;
        let mut bytes_reclaimed: u64 = 0;

        for msg in messages.iter_mut() {
            if msg.role != MessageRole::Tool {
                continue;
            }
            let Some(ref id) = msg.tool_call_id else {
                continue;
            };
            if protected_tool_call_ids.iter().any(|p| p == id) {
                continue;
            }
            if ToolResultPlaceholder::from_placeholder_content(&msg.content).is_ok() {
                // Already a placeholder from an earlier pass; nothing to do.
                continue;
            }

            let (tool_name, turn_id) = id_to_meta
                .get(id)
                .cloned()
                .unwrap_or_else(|| ("unknown_tool".to_string(), 0));

            let age = current_turn.saturating_sub(turn_id);
            let content_len = msg.content.len();
            let oversized = size_threshold != usize::MAX && content_len > size_threshold;
            let stale = age_threshold > 0 && age > age_threshold;

            let reason: Option<&'static str> = match (stale, oversized) {
                (true, _) => Some("tier1_stale"),
                (false, true) => Some("tier1_oversized"),
                _ => None,
            };
            let Some(reason) = reason else { continue };

            let placeholder = ToolResultPlaceholder {
                schema_version: TOOL_RESULT_PLACEHOLDER_SCHEMA_VERSION,
                tool_name,
                tool_call_id: id.clone(),
                turn_id: Some(turn_id),
                original_byte_len: Some(content_len as u64),
                reason: reason.to_string(),
            };
            let replacement = placeholder.to_placeholder_content();
            bytes_reclaimed += content_len.saturating_sub(replacement.len()) as u64;
            msg.content = replacement;
            results_pruned += 1;
        }

        Tier1Report {
            results_pruned,
            bytes_reclaimed,
        }
    }
}

/// Outcome of a single tier-1 pass.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tier1Report {
    /// Number of tool results whose content was content-cleared.
    pub results_pruned: usize,
    /// Bytes saved by swapping out original content for a placeholder.
    pub bytes_reclaimed: u64,
}

impl Tier1Report {
    /// Whether the pass actually performed any work.
    pub fn performed(&self) -> bool {
        self.results_pruned > 0
    }
}

// ─── Tier 2: ApiMicroCompactionConfig ────────────────────────────────────────

/// Default turns to keep when the tier-2 header is enabled.
pub const DEFAULT_TIER2_KEEP_LAST_N_TURNS: u32 = 10;

/// Anthropic server-side tool-use clearing request BUILDER (tier 2).
///
/// This is deliberately **not** a runtime loop.  The Claude Code inspiration
/// that motivates this tier — `apiMicrocompact` / `clear_tool_uses_20250919`
/// — is a request-time decoration: the client opts in by attaching a
/// `context_management` JSON payload to its API request and lets the server
/// prune stale tool uses on its side.  We emit exactly that payload; we do
/// not try to replicate Anthropic's server-side clearing logic ourselves.
///
/// When [`Self::enabled`] is `false` (the default), [`Self::into_context_management_json`]
/// returns `None` and the agent loop sends no additional payload.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApiMicroCompactionConfig {
    /// Opt-in flag.  Defaults to `false` so environments where the Anthropic
    /// server does not yet accept the header keep the old behaviour.
    pub enabled: bool,
    /// Translated to `keep.value` inside the payload. `keep.type` is fixed
    /// to `"tool_uses"` because that is the unit Anthropic's server-side
    /// header operates on.
    pub keep_last_n_turns: u32,
    /// If `false`, the caller opts out of the Anthropic header even when
    /// `enabled` is `true`.  Useful for A/B gating without flipping the
    /// canonical config flag.
    pub emit_clear_tool_uses_header: bool,
}

impl Default for ApiMicroCompactionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            keep_last_n_turns: DEFAULT_TIER2_KEEP_LAST_N_TURNS,
            emit_clear_tool_uses_header: true,
        }
    }
}

impl ApiMicroCompactionConfig {
    /// Enable the builder and leave the rest at the defaults.
    pub fn enabled() -> Self {
        Self {
            enabled: true,
            ..Self::default()
        }
    }

    pub fn with_keep_last_n_turns(mut self, keep: u32) -> Self {
        self.keep_last_n_turns = keep;
        self
    }

    pub fn with_emit_clear_tool_uses_header(mut self, emit: bool) -> Self {
        self.emit_clear_tool_uses_header = emit;
        self
    }

    /// Build the opaque `context_management` payload.  Returns `None` when
    /// tier 2 is disabled (or the header has been explicitly suppressed) so
    /// the caller can safely merge it into `ChatConfig.context_management`
    /// without introducing noise.
    pub fn into_context_management_json(&self) -> Option<serde_json::Value> {
        if !self.enabled || !self.emit_clear_tool_uses_header {
            return None;
        }
        Some(serde_json::json!({
            "edits": [
                {
                    "type": "clear_tool_uses_20250919",
                    "keep": {
                        "type": "tool_uses",
                        "value": self.keep_last_n_turns,
                    }
                }
            ]
        }))
    }

    /// Build a `(provider_name, payload)` pair that a call-site can feed into
    /// `build_tier2_payload_for`.  Separate helper so tests can assert the
    /// provider gating without instantiating a full agent.
    pub fn payload_for_provider(&self, provider_name: &str) -> Option<serde_json::Value> {
        if !is_anthropic_provider(provider_name) {
            return None;
        }
        self.into_context_management_json()
    }
}

/// Classifier used by [`ApiMicroCompactionConfig::payload_for_provider`] and
/// [`TieredCompactionRunner::build_tier2_payload_for`].  Exposed so tests can
/// exercise it directly.
pub fn is_anthropic_provider(provider_name: &str) -> bool {
    // Registry labels sometimes include upstream aliases (`zai`, `r9s`,
    // `glm`, `any`, `bedrock-anthropic`, etc.) that still speak the
    // Anthropic wire format.  Rather than hard-coding every alias we treat
    // any label that *contains* `anthropic` or equals `claude` as
    // Anthropic-compatible.  Unknown vendors default to OFF so tier 2 is
    // never accidentally emitted to OpenAI/Gemini.
    let lowered = provider_name.to_ascii_lowercase();
    lowered == "anthropic" || lowered.contains("anthropic") || lowered == "claude"
}

// ─── Tier 3: FullCompactor trait ─────────────────────────────────────────────

/// Wrapper trait around the existing [`FullCompactionRunner`].  Tier 3 is
/// already implemented in `crate::compaction`; this trait only exists so the
/// [`TieredCompactionRunner`] has a single pluggable surface that tests can
/// substitute without booting the full policy stack.
pub trait FullCompactor: Send + Sync {
    /// Return `Some(tokens)` when the conversation exceeds the threshold at
    /// which tier 3 should fire, and `None` otherwise.  Wraps the existing
    /// `CompactionRunner::needs_preflight`.
    fn needs_compaction(&self, messages: &[Message]) -> Option<u32>;

    /// Perform tier 3.  Delegates to the existing runner and returns the raw
    /// outcome so callers can surface metrics.
    fn compact(&self, messages: &mut Vec<Message>, phase: CompactionPhase) -> CompactionOutcome;
}

impl FullCompactor for FullCompactionRunner {
    fn needs_compaction(&self, messages: &[Message]) -> Option<u32> {
        self.needs_preflight(messages)
    }

    fn compact(&self, messages: &mut Vec<Message>, phase: CompactionPhase) -> CompactionOutcome {
        self.run(messages, phase)
    }
}

/// Outcome of a tier-3 pass, exposed so callers can record metrics without
/// reaching into the inner [`CompactionOutcome`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tier3Report {
    pub performed: bool,
    pub messages_dropped: usize,
    pub tool_results_replaced: usize,
    pub tokens_before: u32,
    pub tokens_after: u32,
    pub summarizer_kind: &'static str,
}

impl From<CompactionOutcome> for Tier3Report {
    fn from(o: CompactionOutcome) -> Self {
        Self {
            performed: o.performed,
            messages_dropped: o.messages_dropped,
            tool_results_replaced: o.tool_results_replaced,
            tokens_before: o.tokens_before,
            tokens_after: o.tokens_after,
            summarizer_kind: o.summarizer_kind,
        }
    }
}

// ─── Three-tier runner ───────────────────────────────────────────────────────

/// Unified three-tier compaction runner.
///
/// The runner only owns configuration/behaviour; it never mutates an agent.
/// Call sites wire the tiers independently:
///
/// * tier 1: `runner.run_tier1(&mut messages, &protected_ids)` at the top of
///   every loop iteration after the previous response lands.
/// * tier 2: `runner.build_tier2_payload_for(provider_name)` at request-
///   build time. Merge the returned JSON into
///   `ChatConfig.context_management` for Anthropic; drop on the floor for
///   other providers.
/// * tier 3: `runner.maybe_run_tier3(&mut messages, phase)` at the budget
///   threshold (today's trigger path — nothing there changes).
pub struct TieredCompactionRunner {
    tier1: MicroCompactionPolicy,
    tier2: ApiMicroCompactionConfig,
    tier3: Box<dyn FullCompactor>,
}

impl std::fmt::Debug for TieredCompactionRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TieredCompactionRunner")
            .field("tier1", &self.tier1)
            .field("tier2", &self.tier2)
            .field("tier3", &"<dyn FullCompactor>")
            .finish()
    }
}

impl TieredCompactionRunner {
    /// Build a runner from explicit tier configuration.
    pub fn new(
        tier1: MicroCompactionPolicy,
        tier2: ApiMicroCompactionConfig,
        tier3: Box<dyn FullCompactor>,
    ) -> Self {
        Self {
            tier1,
            tier2,
            tier3,
        }
    }

    /// Access the tier 1 policy.
    pub fn tier1(&self) -> &MicroCompactionPolicy {
        &self.tier1
    }

    /// Access the tier 2 config.
    pub fn tier2(&self) -> &ApiMicroCompactionConfig {
        &self.tier2
    }

    /// Run tier 1 in-place over `messages`, skipping any tool results whose
    /// `tool_call_id` appears in `protected_tool_call_ids`.
    pub fn run_tier1(
        &self,
        messages: &mut [Message],
        protected_tool_call_ids: &[String],
    ) -> Tier1Report {
        self.tier1.prune(messages, protected_tool_call_ids)
    }

    /// Build the opaque tier 2 payload without considering provider gating.
    /// Call-sites that know the provider is Anthropic can use this; every
    /// other caller should prefer [`Self::build_tier2_payload_for`].
    pub fn build_tier2_payload(&self) -> Option<serde_json::Value> {
        self.tier2.into_context_management_json()
    }

    /// Build the tier 2 payload only if `provider_name` is Anthropic-flavoured.
    pub fn build_tier2_payload_for(&self, provider_name: &str) -> Option<serde_json::Value> {
        self.tier2.payload_for_provider(provider_name)
    }

    /// Run tier 3 when the underlying [`FullCompactor`] reports the
    /// conversation exceeds its threshold. Returns `None` when tier 3 does
    /// not fire so the caller can emit a `no-op` metric.
    pub fn maybe_run_tier3(
        &self,
        messages: &mut Vec<Message>,
        phase: CompactionPhase,
    ) -> Option<Tier3Report> {
        self.tier3.needs_compaction(messages)?;
        let outcome = self.tier3.compact(messages, phase);
        Some(outcome.into())
    }

    /// M8.4/M8.5 fix-first item 7: tier-3 compaction boundary hook.
    ///
    /// When tier 3 fires, the old tool-result messages containing
    /// `[FILE_UNCHANGED]` stubs are pruned/summarised. The matching
    /// entries in the [`crate::file_state_cache::FileStateCache`] must
    /// be cleared so a subsequent `read_file` does not short-circuit
    /// against stale identity. The M8.4 docs promised this; the fix-
    /// first checklist pins it.
    ///
    /// Callers that attach both the tiered runner and a file-state
    /// cache should follow a tier-3 run with a
    /// `cache.clear()` call — this helper performs the conditional
    /// clear inline so the contract is easier to adopt.
    pub fn run_tier3_and_invalidate_cache(
        &self,
        messages: &mut Vec<Message>,
        phase: CompactionPhase,
        file_state_cache: Option<&std::sync::Arc<crate::file_state_cache::FileStateCache>>,
    ) -> Option<Tier3Report> {
        let report = self.maybe_run_tier3(messages, phase)?;
        // Tier 3 fired — clear the cache unconditionally. Partial
        // invalidation would require tracking which files the pruned
        // messages referenced; until that arrives, dropping the whole
        // cache is the correctness-first policy.
        if let Some(cache) = file_state_cache {
            cache.clear();
        }
        Some(report)
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compaction::{CompactionPolicy, CompactionRunner as FullCompactionRunner};
    use octos_core::ToolCall;

    fn user_msg(content: &str) -> Message {
        Message {
            role: MessageRole::User,
            content: content.to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
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
                arguments: serde_json::json!({}),
                metadata: None,
            }]),
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
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
            timestamp: chrono::Utc::now(),
        }
    }

    fn tiered_runner(
        tier1: MicroCompactionPolicy,
        tier2: ApiMicroCompactionConfig,
    ) -> TieredCompactionRunner {
        // Tier 3 is only used by maybe_run_tier3 and the integration test; a
        // stock runner with a tiny budget is enough to exercise its surface
        // without pulling in policy wiring.
        let policy = CompactionPolicy::default();
        let tier3: Box<dyn FullCompactor> = Box::new(FullCompactionRunner::new(policy));
        TieredCompactionRunner::new(tier1, tier2, tier3)
    }

    #[test]
    fn should_prune_tool_results_older_than_max_age() {
        // 6 user turns; keep_age=2 so turns 1..=4 are stale.
        let mut messages = vec![user_msg("turn-1")];
        for i in 2..=6 {
            messages.push(assistant_tool_call("read_file", &format!("call_{i}")));
            messages.push(tool_result(&format!("call_{i}"), &format!("content-{i}")));
            messages.push(user_msg(&format!("turn-{i}")));
        }

        let policy = MicroCompactionPolicy::default()
            .with_max_age_turns(2)
            .with_max_size_bytes_per_result(u32::MAX);
        let report = policy.prune(&mut messages, &[]);

        assert!(report.performed(), "some results should have been pruned");
        // Stale tool results (call_2..call_4) should now hold the placeholder.
        for i in 2..=4 {
            let id = format!("call_{i}");
            let tool = messages
                .iter()
                .find(|m| m.tool_call_id.as_deref() == Some(&id))
                .expect("tool result present");
            assert!(
                ToolResultPlaceholder::from_placeholder_content(&tool.content).is_ok(),
                "call_{i} content was not content-cleared: {:?}",
                tool.content
            );
        }
        // Recent tool results (call_5, call_6) stay intact.
        for i in 5..=6 {
            let id = format!("call_{i}");
            let tool = messages
                .iter()
                .find(|m| m.tool_call_id.as_deref() == Some(&id))
                .expect("tool result present");
            assert_eq!(tool.content, format!("content-{i}"));
        }
    }

    #[test]
    fn should_clear_oversized_tool_results_to_placeholder() {
        let mut messages = vec![
            user_msg("q"),
            assistant_tool_call("shell", "call_big"),
            tool_result("call_big", &"x".repeat(50_000)),
        ];
        // Disable the age-based pruning so only the size path fires.
        let policy = MicroCompactionPolicy::default()
            .with_max_age_turns(u32::MAX)
            .with_max_size_bytes_per_result(1024);
        let report = policy.prune(&mut messages, &[]);
        assert_eq!(report.results_pruned, 1);
        assert!(report.bytes_reclaimed > 45_000);
        let tool = &messages[2];
        let parsed = ToolResultPlaceholder::from_placeholder_content(&tool.content)
            .expect("placeholder round-trips");
        assert_eq!(parsed.tool_call_id, "call_big");
        assert_eq!(parsed.original_byte_len, Some(50_000));
        assert_eq!(parsed.reason, "tier1_oversized");
    }

    #[test]
    fn should_preserve_tool_call_id_on_pruned_results() {
        let mut messages = vec![
            user_msg("q"),
            assistant_tool_call("shell", "call_alpha"),
            tool_result("call_alpha", &"y".repeat(50_000)),
            user_msg("q2"),
        ];
        let policy = MicroCompactionPolicy::default()
            .with_max_age_turns(u32::MAX)
            .with_max_size_bytes_per_result(1024);
        policy.prune(&mut messages, &[]);
        let tool = &messages[2];
        assert_eq!(
            tool.tool_call_id.as_deref(),
            Some("call_alpha"),
            "tool_call_id must survive the prune"
        );
    }

    #[test]
    fn should_skip_tool_results_referenced_by_retry_bucket() {
        // The caller hands a protected set of IDs (e.g. from a pending
        // retry bucket or contract-gated artifact).  Tier 1 must leave
        // those tool results fully intact.
        let mut messages = vec![user_msg("turn-1")];
        for i in 2..=6 {
            messages.push(assistant_tool_call("shell", &format!("call_{i}")));
            messages.push(tool_result(&format!("call_{i}"), &format!("content-{i}")));
            messages.push(user_msg(&format!("turn-{i}")));
        }

        let protected = vec!["call_2".to_string(), "call_4".to_string()];
        let policy = MicroCompactionPolicy::default()
            .with_max_age_turns(1)
            .with_max_size_bytes_per_result(u32::MAX);
        policy.prune(&mut messages, &protected);

        for id in &protected {
            let tool = messages
                .iter()
                .find(|m| m.tool_call_id.as_deref() == Some(id))
                .expect("protected tool result still present");
            assert!(
                !tool
                    .content
                    .starts_with(crate::compaction::TOOL_RESULT_PLACEHOLDER_PREFIX),
                "protected {id} was incorrectly pruned: {:?}",
                tool.content
            );
        }
    }

    #[test]
    fn should_report_bytes_reclaimed_and_count_pruned() {
        let mut messages = vec![
            user_msg("q"),
            assistant_tool_call("tool_a", "call_a"),
            tool_result("call_a", &"a".repeat(20_000)),
            assistant_tool_call("tool_b", "call_b"),
            tool_result("call_b", &"b".repeat(20_000)),
            user_msg("q2"),
        ];
        let policy = MicroCompactionPolicy::default()
            .with_max_age_turns(u32::MAX)
            .with_max_size_bytes_per_result(1024);
        let report = policy.prune(&mut messages, &[]);
        assert_eq!(report.results_pruned, 2);
        // bytes_reclaimed is at least 2*(content-placeholder) bytes, well
        // over 30KB total.
        assert!(report.bytes_reclaimed > 30_000);
    }

    #[test]
    fn should_build_tier2_payload_only_when_enabled() {
        let disabled = ApiMicroCompactionConfig::default();
        assert!(disabled.into_context_management_json().is_none());

        let enabled = ApiMicroCompactionConfig::enabled().with_keep_last_n_turns(7);
        let payload = enabled
            .into_context_management_json()
            .expect("payload emitted when enabled");
        assert_eq!(payload["edits"][0]["type"], "clear_tool_uses_20250919");
        assert_eq!(payload["edits"][0]["keep"]["value"], 7);
        assert_eq!(payload["edits"][0]["keep"]["type"], "tool_uses");

        let suppressed =
            ApiMicroCompactionConfig::enabled().with_emit_clear_tool_uses_header(false);
        assert!(
            suppressed.into_context_management_json().is_none(),
            "header suppression must override the enabled flag"
        );
    }

    #[test]
    fn should_skip_tier2_payload_for_non_anthropic_providers() {
        let config = ApiMicroCompactionConfig::enabled();
        assert!(
            config.payload_for_provider("openai").is_none(),
            "OpenAI must not receive the Anthropic header"
        );
        assert!(
            config.payload_for_provider("gemini").is_none(),
            "Gemini must not receive the Anthropic header"
        );
        assert!(
            config.payload_for_provider("openrouter").is_none(),
            "openrouter proxies many vendors; safest default is OFF"
        );
        assert!(config.payload_for_provider("anthropic").is_some());
        assert!(
            config.payload_for_provider("bedrock-anthropic").is_some(),
            "AWS Bedrock Claude speaks the Anthropic wire format"
        );
    }

    #[test]
    fn should_treat_tier1_as_no_op_when_both_thresholds_inactive() {
        let mut messages = vec![
            user_msg("q"),
            assistant_tool_call("tool", "call_1"),
            tool_result("call_1", &"x".repeat(16_000)),
        ];
        let policy = MicroCompactionPolicy {
            max_age_turns: 0,
            max_size_bytes_per_result: u32::MAX,
        };
        let report = policy.prune(&mut messages, &[]);
        assert_eq!(report, Tier1Report::default());
        assert_eq!(messages[2].content.len(), 16_000);
    }

    #[test]
    fn should_expose_tiered_runner_api() {
        let runner = tiered_runner(
            MicroCompactionPolicy::default(),
            ApiMicroCompactionConfig::enabled(),
        );
        assert_eq!(runner.tier1().max_age_turns, DEFAULT_TIER1_MAX_AGE_TURNS);
        assert!(runner.tier2().enabled);
        assert!(runner.build_tier2_payload_for("anthropic").is_some());
        assert!(runner.build_tier2_payload_for("openai").is_none());
    }

    #[test]
    fn should_skip_tier3_when_below_threshold() {
        // Small conversation -> CompactionRunner.needs_preflight == None so
        // maybe_run_tier3 returns None cleanly.
        let runner = tiered_runner(
            MicroCompactionPolicy::default(),
            ApiMicroCompactionConfig::default(),
        );
        let mut messages = vec![user_msg("hi")];
        let out = runner.maybe_run_tier3(&mut messages, CompactionPhase::OnDemand);
        assert!(out.is_none(), "tier 3 should not fire for tiny convos");
    }

    #[test]
    fn should_preserve_placeholder_idempotency() {
        // Running tier 1 twice on the same messages must be a no-op on the
        // second pass (the placeholder marker prefix is recognised).
        let mut messages = vec![
            user_msg("q"),
            assistant_tool_call("tool", "call_1"),
            tool_result("call_1", &"z".repeat(50_000)),
        ];
        let policy = MicroCompactionPolicy::default()
            .with_max_age_turns(u32::MAX)
            .with_max_size_bytes_per_result(1024);
        let first = policy.prune(&mut messages, &[]);
        assert_eq!(first.results_pruned, 1);
        let second = policy.prune(&mut messages, &[]);
        assert_eq!(second.results_pruned, 0);
    }
}
