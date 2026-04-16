//! Context window trimming and fallback truncation.

use octos_core::{Message, MessageRole};
use tracing::{info, warn};

use super::Agent;

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
