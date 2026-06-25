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

#[cfg(test)]
mod tests {
    use octos_llm::{ChatResponse, StopReason, TokenUsage};
    use serde_json::json;

    use super::*;

    fn resp(content: &str, tools: usize, reasoning: Option<&str>) -> ChatResponse {
        ChatResponse {
            content: Some(content.to_string()),
            reasoning_content: reasoning.map(|s| s.to_string()),
            tool_calls: (0..tools)
                .map(|i| octos_core::ToolCall {
                    id: format!("call_{i}"),
                    name: "dummy".to_string(),
                    arguments: json!({}),
                    metadata: None,
                })
                .collect(),
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
            provider_index: None,
        }
    }

    #[test]
    fn should_be_empty_when_blank_content_no_tools_even_with_reasoning() {
        assert!(is_voice_empty_response(&resp(
            "   ",
            0,
            Some("thinking...")
        )));
    }

    #[test]
    fn should_not_be_empty_when_has_visible_content() {
        assert!(!is_voice_empty_response(&resp("hi", 0, None)));
    }

    #[test]
    fn should_not_be_empty_when_has_tool_calls() {
        assert!(!is_voice_empty_response(&resp("", 1, None)));
    }
}
