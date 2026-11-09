//! Voice-turn failure projection. Additive: does NOT replace the agent
//! loop's original `eyre::Report` — the text path still returns/relies on it.

use crate::harness_errors::HarnessError;
use octos_llm::ChatResponse;

/// Cloneable projection sent to the voice closeout (via a typed oneshot).
#[derive(Clone, Debug)]
pub enum TurnFailure {
    /// A classified LLM call error. `raw_detail` carries the original
    /// rendered message for the voice template's raw-string fallback.
    LlmError {
        error: HarnessError,
        raw_detail: String,
    },
    /// Model returned no visible content and no tool calls (not an error).
    EmptyResponse,
}

/// Voice definition of an empty response: no visible content and no tool
/// calls. Reasoning-only is treated as empty (spoken UX wants real words).
pub fn is_voice_empty_response(resp: &ChatResponse) -> bool {
    resp.content.as_deref().unwrap_or("").trim().is_empty() && resp.tool_calls.is_empty()
}
