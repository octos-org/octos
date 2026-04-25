//! Context window trimming and fallback truncation.

use octos_core::{Message, MessageRole};
use tracing::{info, warn};

use super::Agent;
use crate::compaction::CompactionPhase;
use crate::compaction_tiered::Tier1Report;

impl Agent {
    pub(super) fn trim_to_context_window(&self, messages: &mut Vec<Message>) -> bool {
        use crate::compaction::{MIN_RECENT_MESSAGES, compact_messages, find_recent_boundary};
        use octos_llm::context::{estimate_message_tokens, estimate_tokens};

        if messages.len() <= 1 + MIN_RECENT_MESSAGES {
            return false;
        }

        let window = self.llm.context_window();
        let budget = (window as f64 * 0.8 / crate::compaction::SAFETY_MARGIN) as u32;

        let total: u32 = messages.iter().map(estimate_message_tokens).sum();
        if total <= budget {
            return false;
        }

        let system_tokens = estimate_message_tokens(&messages[0]);
        if system_tokens >= budget {
            warn!(
                system_tokens,
                budget, "system prompt exceeds context window budget, cannot trim"
            );
            return false;
        }

        let split = find_recent_boundary(messages, budget, system_tokens);
        let recent_tokens: u32 = messages[split..].iter().map(estimate_message_tokens).sum();

        // If recent messages alone exceed budget, fall back to simple truncation
        if system_tokens + recent_tokens >= budget {
            return self.fallback_truncate(messages, budget);
        }

        let old_messages = &messages[1..split];
        if old_messages.is_empty() {
            return false;
        }

        let summary_budget = budget - system_tokens - recent_tokens;
        let summary_text = compact_messages(old_messages, summary_budget);
        let summary_tokens = estimate_tokens(&summary_text) + 4;

        let original_count = messages.len();
        let dropped = split - 1;
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
                timestamp: chrono::Utc::now(),
            },
        );

        info!(
            original_tokens = total,
            summary_tokens,
            messages_compacted = dropped,
            messages_remaining = messages.len(),
            original_messages = original_count,
            "compacted conversation history ({} token budget)",
            budget
        );
        true
    }

    /// Run preflight compaction before the first LLM call if the wired
    /// policy declares a threshold and the conversation already exceeds it.
    ///
    /// No-op when no [`crate::compaction::CompactionRunner`] is attached,
    /// which preserves legacy extractive behaviour for every existing caller.
    pub(super) fn maybe_run_preflight_compaction(&self, messages: &mut Vec<Message>) {
        let Some(runner) = self.compaction_runner.as_ref() else {
            return;
        };
        if runner.needs_preflight(messages).is_none() {
            return;
        }
        let outcome = runner.run(messages, CompactionPhase::Preflight);
        info!(
            phase = "preflight",
            performed = outcome.performed,
            messages_dropped = outcome.messages_dropped,
            tool_results_replaced = outcome.tool_results_replaced,
            tokens_before = outcome.tokens_before,
            tokens_after = outcome.tokens_after,
            summarizer = outcome.summarizer_kind,
            "harness M6.3 compaction preflight fired"
        );
        self.enforce_preservation(messages, CompactionPhase::Preflight);
    }

    /// M8.5 tier 1: cheap per-turn micro-compaction.  Runs the
    /// [`crate::compaction_tiered::MicroCompactionPolicy`] in-place across the
    /// current message list so the next LLM request inherits placeholder-
    /// shaped tool results instead of the original payloads. Runs only when a
    /// [`crate::compaction_tiered::TieredCompactionRunner`] is wired.
    ///
    /// Callers must pass `protected_tool_call_ids` — any tool_call_id listed
    /// there is left untouched, preserving the M6 contract-gated artifact
    /// guarantees for pending retry buckets.
    pub(super) fn run_tier1_compaction(
        &self,
        messages: &mut [Message],
        protected_tool_call_ids: &[String],
    ) -> Tier1Report {
        let Some(runner) = self.tiered_compaction.as_ref() else {
            return Tier1Report::default();
        };
        let report = runner.run_tier1(messages, protected_tool_call_ids);
        if report.performed() {
            info!(
                results_pruned = report.results_pruned,
                bytes_reclaimed = report.bytes_reclaimed,
                protected = protected_tool_call_ids.len(),
                "harness M8.5 tier-1 micro-compaction fired"
            );
            metrics::counter!(
                "octos_tier1_compaction_pruned_total",
                "scope" => "tool_results".to_string(),
            )
            .increment(report.results_pruned as u64);
        }
        report
    }

    /// M8.5 tier 2: build the opaque `context_management` payload when the
    /// attached [`crate::compaction_tiered::TieredCompactionRunner`] has the
    /// feature enabled and the active provider speaks the Anthropic wire
    /// format.  Call-sites merge the returned JSON into
    /// `ChatConfig.context_management`; returning `None` means the request
    /// should be sent untouched.
    pub(super) fn build_tier2_context_management(&self) -> Option<serde_json::Value> {
        let runner = self.tiered_compaction.as_ref()?;
        runner.build_tier2_payload_for(self.llm.provider_name())
    }

    /// Run declarative compaction per-iteration (after M0 message prep). Only
    /// active when a [`crate::compaction::CompactionRunner`] is wired; a
    /// no-op otherwise so every caller that does not wire the contract keeps
    /// the existing behaviour byte-for-byte.
    pub(super) fn maybe_run_turn_compaction(&self, messages: &mut Vec<Message>, iteration: u32) {
        let Some(runner) = self.compaction_runner.as_ref() else {
            return;
        };
        // Skip the very first iteration when the preflight path already ran
        // — preflight emits its own events and enforces preservation.
        if iteration == 1 {
            return;
        }
        let outcome = runner.run(messages, CompactionPhase::TurnEnd);
        if outcome.performed {
            info!(
                phase = "turn_end",
                iteration,
                messages_dropped = outcome.messages_dropped,
                tool_results_replaced = outcome.tool_results_replaced,
                tokens_before = outcome.tokens_before,
                tokens_after = outcome.tokens_after,
                summarizer = outcome.summarizer_kind,
                "harness M6.3 compaction per-turn pass"
            );
            self.enforce_preservation(messages, CompactionPhase::TurnEnd);
        }
    }

    /// Run the post-compaction validator rail against the declared
    /// `preserved_artifacts` + `preserved_invariants`. Failures emit a warning
    /// so operators can surface a typed block upstream; the current loop does
    /// not abort mid-turn because M0 guarantees the legacy extractive path
    /// would have been a no-op for the same inputs.
    fn enforce_preservation(&self, messages: &[Message], phase: CompactionPhase) {
        let Some(runner) = self.compaction_runner.as_ref() else {
            return;
        };
        let Some(workspace) = self.compaction_workspace.as_ref() else {
            return;
        };
        match runner.check_preserved(messages, workspace) {
            Ok(ledger) => {
                if !ledger.all_preserved() {
                    let missing: Vec<&str> = ledger.missing.iter().map(|art| art.name()).collect();
                    warn!(
                        phase = phase.as_str(),
                        missing_count = missing.len(),
                        missing = %missing.join(","),
                        "harness M6.3 compaction validator: declared artifacts/invariants were dropped"
                    );
                    metrics::counter!(
                        "octos_compaction_preservation_violations_total",
                        "phase" => phase.as_str().to_string(),
                    )
                    .increment(missing.len() as u64);
                }
            }
            Err(err) => {
                warn!(error = %err, "harness M6.3 compaction validator failed");
            }
        }
    }

    /// Simple truncation fallback when even recent messages exceed budget.
    pub(super) fn fallback_truncate(&self, messages: &mut Vec<Message>, limit: u32) -> bool {
        let system_tokens = octos_llm::context::estimate_message_tokens(&messages[0]);
        let mut kept_tokens = system_tokens;
        let mut keep_from = messages.len();

        for i in (1..messages.len()).rev() {
            let msg_tokens = octos_llm::context::estimate_message_tokens(&messages[i]);
            if kept_tokens + msg_tokens > limit {
                break;
            }
            kept_tokens += msg_tokens;
            keep_from = i;
        }

        // Keep at least 2 non-system messages
        let max_keep_from = messages.len().saturating_sub(2);
        if keep_from > max_keep_from {
            keep_from = max_keep_from;
        }

        // Don't split inside a tool-call group
        while keep_from > 1 && messages[keep_from].role == MessageRole::Tool {
            keep_from -= 1;
        }

        if keep_from > 1 {
            let dropped = keep_from - 1;
            messages.drain(1..keep_from);
            warn!(
                messages_dropped = dropped,
                messages_kept = messages.len(),
                "fallback truncation ({} token limit)",
                limit
            );
            return dropped > 0;
        }
        false
    }
}
