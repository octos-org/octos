//! Compaction summarizer seam (harness M6.3).
//!
//! Compaction turns a block of old conversation messages into a bounded
//! summary. Different strategies live behind a shared trait so the runtime
//! can swap implementations without reshuffling the agent loop:
//!
//! - [`ExtractiveSummarizer`] — deterministic, dependency-free. Preserves the
//!   existing M0 behaviour (header + per-message first-line extraction +
//!   tool-argument stripping). Default for every caller until M6.4 lands.
//! - LLM-iterative summarizer — added in M6.4. Receives the same trait
//!   signature and must produce text bounded by `budget_tokens` so the
//!   runtime treats it interchangeably.
//!
//! The trait is deliberately minimal: take the old messages plus a budget,
//! hand back a `Result<String>`. Failures fall back to the extractive path
//! so no single implementation can break the loop.

use eyre::Result;
use octos_core::Message;

use crate::compaction::compact_messages;

/// Seam for compaction summarization strategies.
///
/// Implementors take a slice of messages to compact and a `budget_tokens`
/// ceiling, and return a text summary whose token estimate stays at or below
/// the budget. Deterministic implementations should be pure. Async-only
/// implementations (e.g. an LLM summarizer) can block on `tokio::runtime::Handle::current`
/// if necessary — the signature stays synchronous so the agent loop can run
/// compaction without awaiting inside the message-prep pipeline.
///
/// The trait must be `Send + Sync` so the runtime can keep summarizers in
/// `Arc<dyn Summarizer>` and share them across spawned worker tasks.
pub trait Summarizer: Send + Sync {
    /// Stable, human-readable identifier for this summarizer strategy.
    ///
    /// Reported in compaction phase events so operators can tell whether a
    /// turn was compacted by the extractive (`"extractive"`) or the
    /// LLM-iterative (`"llm_iterative"`) variant. Keep this lowercase
    /// snake_case so it also serializes cleanly through
    /// `CompactionSummarizerKind`.
    fn kind(&self) -> &'static str;

    /// Return a bounded summary of `messages`.
    ///
    /// Implementors MUST respect `budget_tokens`. The extractive fallback
    /// measures token count via `octos_llm::context::estimate_tokens`, so
    /// approximate adherence is acceptable — but wildly overshooting the
    /// budget is a contract violation and will be rejected by the runtime.
    fn summarize(&self, messages: &[Message], budget_tokens: u32) -> Result<String>;
}

/// Deterministic, dependency-free summarizer that preserves the existing
/// extractive behaviour. Used by default so the absence of a policy leaves
/// the loop indistinguishable from the pre-M6.3 runtime.
#[derive(Debug, Default, Clone, Copy)]
pub struct ExtractiveSummarizer;

impl ExtractiveSummarizer {
    /// Construct a new extractive summarizer.
    pub fn new() -> Self {
        Self
    }
}

impl Summarizer for ExtractiveSummarizer {
    fn kind(&self) -> &'static str {
        "extractive"
    }

    fn summarize(&self, messages: &[Message], budget_tokens: u32) -> Result<String> {
        Ok(compact_messages(messages, budget_tokens))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_core::MessageRole;

    fn user(content: &str) -> Message {
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

    #[test]
    fn extractive_summarizer_reports_stable_kind() {
        assert_eq!(ExtractiveSummarizer::new().kind(), "extractive");
    }

    #[test]
    fn extractive_summarizer_produces_nonempty_summary_within_budget() {
        let messages = vec![user("hello"), user("world")];
        let summary = ExtractiveSummarizer::new()
            .summarize(&messages, 2_000)
            .expect("summarize should succeed");
        assert!(summary.contains("Conversation Summary"));
        assert!(summary.contains("> User: hello"));
    }
}
