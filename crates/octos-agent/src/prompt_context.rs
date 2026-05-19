//! Caller-owned prompt context management bridge.
//!
//! `octos-agent` is intentionally lower-level than the AppUI/session runtime:
//! it must not depend on the CLI crate's durable `ContextManager`. This module
//! defines the small object-safe hook the session runtime can implement so the
//! final model prompt can still be prepared by the server-owned context ledger
//! immediately before each LLM call.

use octos_core::Message;

/// Loop phase at which the prompt context bridge is invoked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptContextPhase {
    /// First model call of a user turn.
    TurnStart,
    /// Subsequent model call after tool execution or another loop iteration.
    Iteration,
    /// Retry path after the loop classifier requested context compaction.
    Retry,
}

impl PromptContextPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TurnStart => "turn_start",
            Self::Iteration => "iteration",
            Self::Retry => "retry",
        }
    }
}

/// Metadata describing the model prompt being prepared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptContextRequest {
    pub phase: PromptContextPhase,
    pub iteration: u32,
    pub provider_name: String,
    pub model_id: String,
    pub context_window: u32,
}

/// Summary returned by a prompt context bridge invocation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PromptContextReport {
    /// Whether the bridge replaced the message vector passed to the model.
    pub prompt_replaced: bool,
    /// Whether a durable/scratch compaction pass was installed.
    pub compaction_performed: bool,
    pub messages_before: usize,
    pub messages_after: usize,
    pub token_estimate: Option<usize>,
    pub generation: Option<u64>,
}

/// Object-safe bridge implemented by session runtimes that own a canonical
/// context ledger. Returning an error does not abort the agent loop; the agent
/// logs it and falls back to its existing prompt vector.
pub trait PromptContextManager: Send + Sync {
    fn prepare_prompt(
        &self,
        request: PromptContextRequest,
        messages: &mut Vec<Message>,
    ) -> Result<PromptContextReport, String>;
}
