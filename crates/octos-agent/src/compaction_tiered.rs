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

/// Which tier-1 conditions a pass may apply. Split for provider prefix-cache
/// (KV) friendliness: oversized results just landed near the prefix tail —
/// rewriting them is cheap for the cache — while stale results sit deep in
/// history, so rewriting them invalidates the whole cached prefix and is
/// consolidated to one turn-start pass (spec kv-cache-friendly-compaction).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier1Pass {
    /// Per-iteration: clear only oversized results.
    OversizedOnly,
    /// Turn start: clear oversized AND stale results.
    Full,
}

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
    /// #2131 working-set pinning: the most-recently-*touched* (read or
    /// written) distinct files are the active working set — evicting one
    /// mid-task just forces the model to re-read it next turn (the observed
    /// llm.c pathology: the same source file read 66 times). The read/write
    /// results for the newest `pin_recent_files` files are exempt from BOTH
    /// the stale and oversized conditions, so the file the model is actually
    /// working on stays in context. `0` disables pinning (pre-#2131
    /// behaviour). Bounded by design: only K files are pinned, so older
    /// reads still evict and tier-3 can still summarise.
    #[serde(default = "default_pin_recent_files")]
    pub pin_recent_files: u32,
    /// #2131 read dedup: when several tool results read the SAME file+range,
    /// keep only the newest and stub the rest on sight (regardless of age or
    /// size). Collapses the N-stubs-for-one-file clutter a re-read loop
    /// leaves behind. `true` by default; the newest read of each range (and
    /// any pinned file) always survives.
    #[serde(default = "default_dedup_duplicate_reads")]
    pub dedup_duplicate_reads: bool,
}

/// Default number of recently-touched files to pin (#2131).
pub const DEFAULT_TIER1_PIN_RECENT_FILES: u32 = 5;

fn default_pin_recent_files() -> u32 {
    DEFAULT_TIER1_PIN_RECENT_FILES
}

fn default_dedup_duplicate_reads() -> bool {
    true
}

impl Default for MicroCompactionPolicy {
    fn default() -> Self {
        Self {
            max_age_turns: DEFAULT_TIER1_MAX_AGE_TURNS,
            max_size_bytes_per_result: DEFAULT_TIER1_MAX_SIZE_BYTES_PER_RESULT,
            pin_recent_files: DEFAULT_TIER1_PIN_RECENT_FILES,
            dedup_duplicate_reads: true,
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
        self.prune_with_pass(messages, protected_tool_call_ids, Tier1Pass::Full)
    }

    /// [`Self::prune`] with an explicit [`Tier1Pass`]: `OversizedOnly` skips
    /// the age-based (stale) condition so per-iteration runs never rewrite
    /// deep history (KV-cache friendliness); `Full` behaves like `prune`.
    pub fn prune_with_pass(
        &self,
        messages: &mut [Message],
        protected_tool_call_ids: &[String],
        pass: Tier1Pass,
    ) -> Tier1Report {
        // Nothing to do only when EVERY tier-1 lever is inactive — the age and
        // size thresholds AND the #2131 pin/dedup features.
        if self.max_age_turns == 0
            && self.max_size_bytes_per_result == u32::MAX
            && self.pin_recent_files == 0
            && !self.dedup_duplicate_reads
        {
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

        // #2131: which read results are the freshest copy of an actively-used
        // file (pin — never evict) and which are superseded duplicates (dedup —
        // evict on sight). Computed once from the assistant tool calls.
        let working_set =
            WorkingSet::analyze(messages, self.pin_recent_files, self.dedup_duplicate_reads);

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

            // #2131 working-set pin: never evict the freshest read of an
            // actively-used file, even when oversized — evicting it just
            // forces a re-read next turn.
            if working_set.pinned_ids.contains(id) {
                continue;
            }

            let (tool_name, turn_id) = id_to_meta
                .get(id)
                .cloned()
                .unwrap_or_else(|| ("unknown_tool".to_string(), 0));

            let age = current_turn.saturating_sub(turn_id);
            let content_len = msg.content.len();
            let oversized = size_threshold != usize::MAX && content_len > size_threshold;
            let stale = matches!(pass, Tier1Pass::Full) && age_threshold > 0 && age > age_threshold;
            // #2131 dedup: a read superseded by a newer read of the same
            // file+range is pure redundancy — collapse it regardless of age or
            // size. Gated to the Full pass like `stale`, so the per-iteration
            // OversizedOnly pass never rewrites deep history (KV-cache friendly).
            let superseded =
                matches!(pass, Tier1Pass::Full) && working_set.superseded_ids.contains(id);

            let reason: Option<&'static str> = if superseded {
                Some("tier1_superseded")
            } else if stale {
                Some("tier1_stale")
            } else if oversized {
                Some("tier1_oversized")
            } else {
                None
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

/// Tool names whose result carries a file READ (dedup + pin candidates).
fn is_read_tool(name: &str) -> bool {
    matches!(name, "read_file" | "read")
}

/// Tool names whose call TOUCHES (writes/edits) a file — they contribute to
/// the recently-touched working set even though the result itself is usually
/// a small confirmation rather than file content.
fn is_write_tool(name: &str) -> bool {
    matches!(
        name,
        "write_file" | "edit_file" | "diff_edit" | "apply_patch"
    )
}

/// The file path a read/write tool call targets, from its arguments. Covers
/// the `path`, `file_path`, and `filePath` conventions — `read_file` accepts
/// the camelCase `filePath` alias (#1767), so missing it would silently leave
/// alias-style reads unpinned/undeduped.
fn tool_target_path(args: &serde_json::Value) -> Option<String> {
    args.get("path")
        .or_else(|| args.get("file_path"))
        .or_else(|| args.get("filePath"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// A read's (path, start, end, limit) identity for dedup — the same window of
/// the same file is the same read. `read_file`'s range is `start_line`/`offset`
/// plus EITHER `end_line` OR `limit`, so ALL of them belong in the key: two
/// reads of `x` at start 1 with `end_line: 50` vs `end_line: 10` are different
/// windows and must NOT dedup to each other (that would silently drop lines
/// 11-50 and force a re-read — the very thrash this feature prevents).
fn read_range_key(args: &serde_json::Value) -> Option<String> {
    let path = tool_target_path(args)?;
    let start = args
        .get("offset")
        .or_else(|| args.get("start_line"))
        .and_then(serde_json::Value::as_i64);
    let end = args.get("end_line").and_then(serde_json::Value::as_i64);
    let limit = args.get("limit").and_then(serde_json::Value::as_i64);
    Some(format!("{path}\u{1f}{start:?}\u{1f}{end:?}\u{1f}{limit:?}"))
}

/// Working-set analysis of the conversation for one tier-1 pass (#2131):
/// which tool-result ids hold the freshest read of a pinned (recently-touched)
/// file, and which read ids are superseded duplicates.
#[derive(Default)]
struct WorkingSet {
    /// Ids to exempt from eviction: the newest read of each pinned file.
    pinned_ids: std::collections::HashSet<String>,
    /// Read ids that a newer read of the SAME file+range supersedes.
    superseded_ids: std::collections::HashSet<String>,
}

impl WorkingSet {
    /// Analyse assistant tool calls in message order. `pin_recent_files` is K
    /// (0 disables pinning); `dedup` toggles the superseded-read set.
    fn analyze(messages: &[Message], pin_recent_files: u32, dedup: bool) -> Self {
        if pin_recent_files == 0 && !dedup {
            return Self::default();
        }
        // Latest touch order per path, latest read (order,id) per path, and
        // latest read (order,id) per range key — all keyed by encounter order.
        let mut order: usize = 0;
        let mut latest_touch: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        let mut latest_read_by_path: std::collections::HashMap<String, (usize, String)> =
            std::collections::HashMap::new();
        let mut latest_read_by_range: std::collections::HashMap<String, (usize, String)> =
            std::collections::HashMap::new();
        // Every read id with its range key, so we can subtract the survivors.
        let mut reads: Vec<(String, String)> = Vec::new();

        for msg in messages {
            if msg.role != MessageRole::Assistant {
                continue;
            }
            let Some(ref calls) = msg.tool_calls else {
                continue;
            };
            for call in calls {
                let is_read = is_read_tool(&call.name);
                let is_write = is_write_tool(&call.name);
                if !is_read && !is_write {
                    continue;
                }
                let Some(path) = tool_target_path(&call.arguments) else {
                    continue;
                };
                order += 1;
                latest_touch.insert(path.clone(), order);
                if is_read {
                    latest_read_by_path.insert(path.clone(), (order, call.id.clone()));
                    if let Some(range) = read_range_key(&call.arguments) {
                        reads.push((call.id.clone(), range.clone()));
                        latest_read_by_range.insert(range, (order, call.id.clone()));
                    }
                }
            }
        }

        // Pin the newest read of the K most-recently-touched files.
        let mut pinned_ids = std::collections::HashSet::new();
        if pin_recent_files > 0 {
            let mut by_recency: Vec<(&String, &usize)> = latest_touch.iter().collect();
            by_recency.sort_by(|a, b| b.1.cmp(a.1));
            for (path, _) in by_recency.into_iter().take(pin_recent_files as usize) {
                if let Some((_, id)) = latest_read_by_path.get(path) {
                    pinned_ids.insert(id.clone());
                }
            }
        }

        // A read is superseded when it is not the newest read of its range.
        let mut superseded_ids = std::collections::HashSet::new();
        if dedup {
            for (id, range) in reads {
                if latest_read_by_range
                    .get(&range)
                    .is_some_and(|(_, newest)| newest != &id)
                {
                    superseded_ids.insert(id);
                }
            }
        }

        Self {
            pinned_ids,
            superseded_ids,
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
        pass: Tier1Pass,
    ) -> Tier1Report {
        self.tier1
            .prune_with_pass(messages, protected_tool_call_ids, pass)
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
                arguments: serde_json::json!({}),
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

    /// An assistant message issuing one tool call with explicit `arguments`
    /// (so #2131 pin/dedup can read the `path`/`offset`/`limit`).
    fn assistant_call_args(
        tool_name: &str,
        tool_id: &str,
        arguments: serde_json::Value,
    ) -> Message {
        Message {
            role: MessageRole::Assistant,
            content: String::new(),
            media: vec![],
            tool_calls: Some(vec![ToolCall {
                id: tool_id.to_string(),
                name: tool_name.to_string(),
                arguments,
                metadata: None,
            }]),
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        }
    }

    fn is_placeholder(content: &str) -> bool {
        ToolResultPlaceholder::from_placeholder_content(content).is_ok()
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
    fn pins_the_freshest_read_of_the_active_working_file_against_oversize() {
        // #2131: an oversized read of the file the model is actively using must
        // NOT be cleared — evicting it just forces a re-read next turn (the
        // llm.c pathology). K=1 pins only the single most-recently-touched
        // file, so a second, older file's oversized read still evicts.
        let big = "x".repeat(4096);
        let mut messages = vec![
            user_msg("go"),
            assistant_call_args("read_file", "r_old", serde_json::json!({"path": "old.txt"})),
            tool_result("r_old", &big),
            assistant_call_args(
                "read_file",
                "r_active",
                serde_json::json!({"path": "active.rs"}),
            ),
            tool_result("r_active", &big),
        ];
        let policy = MicroCompactionPolicy {
            max_age_turns: 0, // no stale pruning; isolate the size/pin paths
            max_size_bytes_per_result: 1024,
            pin_recent_files: 1,
            dedup_duplicate_reads: false,
        };
        policy.prune(&mut messages, &[]);
        // active.rs is the single pinned file → its (oversized) read survives.
        assert!(
            !is_placeholder(&messages[4].content),
            "the freshest read of the active file must be pinned"
        );
        // old.txt is outside the top-1 working set → oversized read is cleared.
        assert!(
            is_placeholder(&messages[2].content),
            "a non-working-set oversized read still evicts"
        );
    }

    #[test]
    fn writing_a_file_keeps_it_in_the_working_set() {
        // #2131: a file WRITTEN this turn counts as touched, so its earlier
        // read stays pinned (you are actively editing it).
        let big = "x".repeat(4096);
        let mut messages = vec![
            user_msg("go"),
            assistant_call_args("read_file", "r_out", serde_json::json!({"path": "out.rs"})),
            tool_result("r_out", &big),
            assistant_call_args("read_file", "r_ref", serde_json::json!({"path": "ref.txt"})),
            tool_result("r_ref", &big),
            // Now WRITE out.rs — the most recent touch of any file.
            assistant_call_args("write_file", "w_out", serde_json::json!({"path": "out.rs"})),
            tool_result("w_out", "ok"),
        ];
        let policy = MicroCompactionPolicy {
            max_age_turns: 0,
            max_size_bytes_per_result: 1024,
            pin_recent_files: 1,
            dedup_duplicate_reads: false,
        };
        policy.prune(&mut messages, &[]);
        // out.rs is the most-recently-touched file (via the write) → its read
        // is pinned even though ref.txt was read more recently than out.rs.
        assert!(
            !is_placeholder(&messages[2].content),
            "the read of a just-written file must stay pinned"
        );
    }

    #[test]
    fn dedups_superseded_reads_of_the_same_range() {
        // #2131: two reads of the SAME file+range — the older is redundant and
        // collapses to a placeholder; the newest survives.
        let mut messages = vec![
            user_msg("go"),
            assistant_call_args(
                "read_file",
                "r1",
                serde_json::json!({"path": "a.txt", "offset": 0, "limit": 100}),
            ),
            tool_result("r1", "stale content"),
            assistant_call_args(
                "read_file",
                "r2",
                serde_json::json!({"path": "a.txt", "offset": 0, "limit": 100}),
            ),
            tool_result("r2", "fresh content"),
        ];
        let policy = MicroCompactionPolicy {
            max_age_turns: 0,                    // not stale
            max_size_bytes_per_result: u32::MAX, // not oversized
            pin_recent_files: 0,                 // isolate dedup from pinning
            dedup_duplicate_reads: true,
        };
        policy.prune(&mut messages, &[]);
        // r1 (older duplicate) is superseded → cleared with the dedup reason.
        let parsed = ToolResultPlaceholder::from_placeholder_content(&messages[2].content)
            .expect("superseded read is a placeholder");
        assert_eq!(parsed.reason, "tier1_superseded");
        // r2 (newest) survives untouched.
        assert!(!is_placeholder(&messages[4].content));
    }

    #[test]
    fn reads_differing_only_by_end_line_are_not_deduped() {
        // #2131 review: end_line is part of a read's window identity. Two reads
        // of the same file+start but different end_line are DIFFERENT windows
        // and must both survive — deduping them would silently drop content.
        let mut messages = vec![
            user_msg("go"),
            assistant_call_args(
                "read_file",
                "r_wide",
                serde_json::json!({"path": "a.txt", "start_line": 1, "end_line": 50}),
            ),
            tool_result("r_wide", "lines 1-50"),
            assistant_call_args(
                "read_file",
                "r_narrow",
                serde_json::json!({"path": "a.txt", "start_line": 1, "end_line": 10}),
            ),
            tool_result("r_narrow", "lines 1-10"),
        ];
        let policy = MicroCompactionPolicy {
            max_age_turns: 0,
            max_size_bytes_per_result: u32::MAX,
            pin_recent_files: 0, // isolate dedup
            dedup_duplicate_reads: true,
        };
        policy.prune(&mut messages, &[]);
        assert!(
            !is_placeholder(&messages[2].content),
            "the wider read (lines 1-50) must NOT be deduped away by a narrower one"
        );
        assert!(
            !is_placeholder(&messages[4].content),
            "the narrower read survives too"
        );
    }

    #[test]
    fn dedup_is_skipped_in_the_oversized_only_pass() {
        // #2131: dedup rewrites deep history, so like `stale` it runs only in
        // the Full pass — the per-iteration OversizedOnly pass leaves the KV
        // prefix cache intact.
        let mut messages = vec![
            user_msg("go"),
            assistant_call_args("read_file", "r1", serde_json::json!({"path": "a.txt"})),
            tool_result("r1", "old"),
            assistant_call_args("read_file", "r2", serde_json::json!({"path": "a.txt"})),
            tool_result("r2", "new"),
        ];
        let policy = MicroCompactionPolicy {
            max_age_turns: 0,
            max_size_bytes_per_result: u32::MAX,
            pin_recent_files: 0,
            dedup_duplicate_reads: true,
        };
        policy.prune_with_pass(&mut messages, &[], Tier1Pass::OversizedOnly);
        assert!(
            !is_placeholder(&messages[2].content),
            "OversizedOnly must not dedup deep history"
        );
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
            // Every lever off → the pass early-returns as a true no-op.
            pin_recent_files: 0,
            dedup_duplicate_reads: false,
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
    fn oversized_only_pass_never_touches_stale_results() {
        // KV-cache rationale (spec kv-cache-friendly-compaction): age-based
        // rewrites land DEEP in history and invalidate the provider prefix
        // cache; a per-iteration pass may only clear oversized results that
        // just arrived near the prefix tail.
        let policy = MicroCompactionPolicy::default(); // age 5 turns, 8KB
        let mut messages = vec![
            user_msg("turn 1"),
            assistant_tool_call("shell", "call_old"),
            tool_result("call_old", "small old result"),
        ];
        for n in 2..=7 {
            messages.push(user_msg(&format!("turn {n}")));
        }
        messages.push(assistant_tool_call("shell", "call_big"));
        messages.push(tool_result("call_big", &"x".repeat(9 * 1024)));

        let report = policy.prune_with_pass(&mut messages, &[], Tier1Pass::OversizedOnly);

        assert_eq!(report.results_pruned, 1, "only the oversized result clears");
        let old = messages
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("call_old"))
            .unwrap();
        assert!(
            old.content.contains("small old result"),
            "stale-but-small result must survive an oversized-only pass"
        );
    }

    #[test]
    fn full_pass_still_prunes_stale_results() {
        let policy = MicroCompactionPolicy::default();
        let mut messages = vec![
            user_msg("turn 1"),
            assistant_tool_call("shell", "call_old"),
            tool_result("call_old", "small old result"),
        ];
        for n in 2..=7 {
            messages.push(user_msg(&format!("turn {n}")));
        }

        let report = policy.prune_with_pass(&mut messages, &[], Tier1Pass::Full);

        assert_eq!(
            report.results_pruned, 1,
            "full pass prunes the stale result"
        );
    }

    #[test]
    fn protected_ids_survive_the_oversized_only_pass() {
        let policy = MicroCompactionPolicy::default();
        let mut messages = vec![
            user_msg("turn 1"),
            assistant_tool_call("shell", "call_guarded"),
            tool_result("call_guarded", &"y".repeat(9 * 1024)),
        ];

        let report = policy.prune_with_pass(
            &mut messages,
            &["call_guarded".to_string()],
            Tier1Pass::OversizedOnly,
        );

        assert_eq!(report.results_pruned, 0, "protected ids are untouchable");
        let guarded = messages
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("call_guarded"))
            .unwrap();
        assert!(guarded.content.starts_with('y'), "content intact");
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
