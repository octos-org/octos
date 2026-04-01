//! Types for LLM interactions.

use octos_core::ToolCall;
use serde::{Deserialize, Serialize};

/// Response from a chat completion request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    /// Text content of the response (if any).
    pub content: Option<String>,
    /// Reasoning/thinking content from thinking models (kimi-k2.5, o1, etc.).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// Tool calls requested by the model.
    pub tool_calls: Vec<ToolCall>,
    /// Why the model stopped generating.
    pub stop_reason: StopReason,
    /// Token usage statistics.
    pub usage: TokenUsage,
    /// Index of the provider slot that produced this response (set by `ProviderChain`).
    /// Used by `report_late_failure` to penalize the correct provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_index: Option<usize>,
}

/// Why the model stopped generating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// Model finished naturally.
    EndTurn,
    /// Model wants to use tools.
    ToolUse,
    /// Hit max tokens limit.
    MaxTokens,
    /// Hit a stop sequence.
    StopSequence,
    /// Content was blocked by safety/moderation filters.
    /// OpenAI: `content_filter`, Gemini: `SAFETY`/`RECITATION`/`OTHER`.
    ContentFiltered,
}

/// Token usage statistics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// Tokens used for internal chain-of-thought (o1/o3, Claude thinking, Gemini thinking).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub reasoning_tokens: u32,
    /// Tokens served from provider cache (Anthropic cache_control, OpenAI automatic).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cache_read_tokens: u32,
    /// Tokens written to provider cache.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cache_write_tokens: u32,
}

fn is_zero(v: &u32) -> bool {
    *v == 0
}

/// Tool specification for LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    /// Tool name.
    pub name: String,
    /// Description of what the tool does.
    pub description: String,
    /// JSON Schema for the tool's input parameters.
    pub input_schema: serde_json::Value,
}

/// Events from a streaming LLM response.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// Incremental text chunk.
    TextDelta(String),
    /// Incremental reasoning/thinking content from thinking models.
    ReasoningDelta(String),
    /// Incremental tool call data.
    ToolCallDelta {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        arguments_delta: String,
    },
    /// Provider-specific metadata for a tool call (e.g. Gemini thought_signature).
    ToolCallMetadata {
        index: usize,
        metadata: serde_json::Value,
    },
    /// Token usage (sent at stream end by most providers).
    Usage(TokenUsage),
    /// Stream finished with stop reason.
    Done(StopReason),
    /// Error during streaming.
    Error(String),
}

/// A boxed stream of StreamEvents.
pub type ChatStream = std::pin::Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send>>;

/// Strip `<think>…</think>` blocks from LLM content.
///
/// Some models (DeepSeek, MiniMax, Qwen thinking variants) embed chain-of-thought
/// inside `<think>` tags in the main content field instead of using the structured
/// `reasoning_content` field. This extracts the thinking into a separate string
/// and returns the cleaned content.
///
/// Returns `(cleaned_content, extracted_thinking)`.
pub fn strip_think_tags(text: &str) -> (String, Option<String>) {
    let mut thinking = String::new();
    let mut cleaned = String::new();
    let mut rest = text;

    while let Some(start) = rest.find("<think>") {
        // Text before this <think> tag
        cleaned.push_str(&rest[..start]);

        let after_open = &rest[start + "<think>".len()..];
        if let Some(end) = after_open.find("</think>") {
            if !thinking.is_empty() {
                thinking.push('\n');
            }
            thinking.push_str(after_open[..end].trim());
            rest = &after_open[end + "</think>".len()..];
        } else {
            // Unclosed <think> — treat everything after as thinking
            if !thinking.is_empty() {
                thinking.push('\n');
            }
            thinking.push_str(after_open.trim());
            rest = "";
            break;
        }
    }
    cleaned.push_str(rest);

    let cleaned = cleaned.trim().to_string();
    let thinking = if thinking.is_empty() {
        None
    } else {
        Some(thinking)
    };
    (cleaned, thinking)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_think_tags_basic() {
        let (content, thinking) =
            strip_think_tags("<think>reasoning here</think>The answer is 42.");
        assert_eq!(content, "The answer is 42.");
        assert_eq!(thinking.unwrap(), "reasoning here");
    }

    #[test]
    fn test_strip_think_tags_no_tags() {
        let (content, thinking) = strip_think_tags("No thinking tags here.");
        assert_eq!(content, "No thinking tags here.");
        assert!(thinking.is_none());
    }

    #[test]
    fn test_strip_think_tags_empty_think() {
        let (content, thinking) = strip_think_tags("<think>\n\n</think>Just the answer.");
        assert_eq!(content, "Just the answer.");
        assert!(thinking.is_none());
    }

    #[test]
    fn test_strip_think_tags_multiple() {
        let (content, thinking) =
            strip_think_tags("<think>step 1</think>First. <think>step 2</think>Second.");
        assert_eq!(content, "First. Second.");
        assert_eq!(thinking.unwrap(), "step 1\nstep 2");
    }

    #[test]
    fn test_strip_think_tags_unclosed() {
        let (content, thinking) = strip_think_tags("Before <think>unclosed reasoning");
        assert_eq!(content, "Before");
        assert_eq!(thinking.unwrap(), "unclosed reasoning");
    }

    #[test]
    fn test_strip_think_tags_only_think() {
        let (content, thinking) = strip_think_tags("<think>all thinking no content</think>");
        assert_eq!(content, "");
        assert_eq!(thinking.unwrap(), "all thinking no content");
    }
}
