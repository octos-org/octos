//! Types for LLM interactions.

use octos_core::ToolCall;
use serde::{Deserialize, Serialize};

/// Structured provenance for the provider instance that produced a response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderMetadata {
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

impl ProviderMetadata {
    pub fn new(
        provider: impl Into<String>,
        model: impl Into<String>,
        endpoint: Option<String>,
    ) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            endpoint,
        }
    }

    pub fn display_label(&self) -> String {
        match self.endpoint.as_deref().filter(|value| !value.is_empty()) {
            Some(endpoint) => format!("{}/{} @ {}", self.provider, self.model, endpoint),
            None => format!("{}/{}", self.provider, self.model),
        }
    }
}

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
///
/// Cache accounting contract: `cache_read_tokens` / `cache_write_tokens`
/// are DISJOINT from `input_tokens` (Anthropic-style) — the total prompt is
/// `input + cache_read + cache_write`. Anthropic reports this natively;
/// providers whose wire format counts cached tokens INSIDE the prompt total
/// (OpenAI `prompt_tokens_details.cached_tokens`, Gemini
/// `cachedContentTokenCount`) are normalized at their parse boundary by
/// subtracting the cached share from `input_tokens`. Consumers summing
/// "everything processed" must add all three; never re-add cache counts to
/// an inclusive total.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Non-cached prompt tokens billed at the full input rate.
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
    /// Identifies which provider instance produced the stream.
    ProviderIndex(usize),
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

const THINK_OPEN: &str = "<think>";
const THINK_CLOSE: &str = "</think>";

/// Incremental version of [`strip_think_tags`] for STREAMING deltas.
///
/// Models that embed chain-of-thought inline (MiniMax, Qwen, some DeepSeek
/// routes) stream `<think>…</think>` inside ordinary content deltas — the
/// structured reasoning lane never sees it, so a client's thinking-display
/// toggle cannot hide it and raw tags render in live transcripts. The
/// non-streaming parse and the post-stream accumulator both strip after the
/// fact; this splitter routes the spans AS THEY ARRIVE.
///
/// Feed each content delta; get back `(content_part, reasoning_part)`.
/// A suffix that could be the start of a tag split across deltas
/// (`"…<th"` + `"ink>…"`) is held back until it can be classified. Call
/// [`ThinkTagStreamSplitter::finish`] at stream end to flush the holdback;
/// text inside an unclosed `<think>` is treated as reasoning, matching
/// [`strip_think_tags`].
#[derive(Debug, Default)]
pub struct ThinkTagStreamSplitter {
    inside_think: bool,
    /// Holdback: bytes that might be a partial tag prefix split across
    /// deltas. Always shorter than the tag itself.
    pending: String,
}

impl ThinkTagStreamSplitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one streamed content delta. Returns the text to route to the
    /// CONTENT lane and the text to route to the REASONING lane.
    pub fn feed(&mut self, delta: &str) -> (String, String) {
        let mut buf = std::mem::take(&mut self.pending);
        buf.push_str(delta);
        let mut content = String::new();
        let mut reasoning = String::new();
        loop {
            let tag = if self.inside_think {
                THINK_CLOSE
            } else {
                THINK_OPEN
            };
            match buf.find(tag) {
                Some(idx) => {
                    if self.inside_think {
                        reasoning.push_str(&buf[..idx]);
                    } else {
                        content.push_str(&buf[..idx]);
                    }
                    buf.drain(..idx + tag.len());
                    self.inside_think = !self.inside_think;
                }
                None => {
                    // Emit everything except a trailing run that could be
                    // the start of the tag we are looking for.
                    let keep = partial_tag_suffix_len(&buf, tag);
                    let tail = buf.split_off(buf.len() - keep);
                    if self.inside_think {
                        reasoning.push_str(&buf);
                    } else {
                        content.push_str(&buf);
                    }
                    self.pending = tail;
                    break;
                }
            }
        }
        (content, reasoning)
    }

    /// Flush the holdback at stream end. A pending partial tag was literal
    /// text after all; route it to whichever lane the splitter is in.
    pub fn finish(&mut self) -> (String, String) {
        let tail = std::mem::take(&mut self.pending);
        if tail.is_empty() {
            (String::new(), String::new())
        } else if self.inside_think {
            (String::new(), tail)
        } else {
            (tail, String::new())
        }
    }
}

/// Length of the longest buffer SUFFIX that is a proper prefix of `tag`
/// (e.g. buffer `"hello <th"` vs `"<think>"` → 3). Tags are ASCII, so any
/// matching suffix is ASCII and the boundary check keeps multi-byte input
/// safe.
fn partial_tag_suffix_len(buf: &str, tag: &str) -> usize {
    let max = (tag.len() - 1).min(buf.len());
    for len in (1..=max).rev() {
        let start = buf.len() - len;
        if buf.is_char_boundary(start) && tag.starts_with(&buf[start..]) {
            return len;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the splitter with an arbitrary chunking of `parts` and collect
    /// both lanes.
    fn split_parts(parts: &[&str]) -> (String, String) {
        let mut splitter = ThinkTagStreamSplitter::new();
        let mut content = String::new();
        let mut reasoning = String::new();
        for part in parts {
            let (c, r) = splitter.feed(part);
            content.push_str(&c);
            reasoning.push_str(&r);
        }
        let (c, r) = splitter.finish();
        content.push_str(&c);
        reasoning.push_str(&r);
        (content, reasoning)
    }

    #[test]
    fn splitter_routes_think_span_to_reasoning_lane() {
        let (content, reasoning) =
            split_parts(&["<think>plan the review</think>", "The answer is 42."]);
        assert_eq!(content, "The answer is 42.");
        assert_eq!(reasoning, "plan the review");
    }

    #[test]
    fn splitter_handles_tag_split_across_deltas() {
        // The MiniMax live-leak case: tags arrive fragmented across chunks.
        let (content, reasoning) =
            split_parts(&["Hello <th", "ink>hidden ", "chain</thin", "k> world"]);
        assert_eq!(content, "Hello  world");
        assert_eq!(reasoning, "hidden chain");
    }

    #[test]
    fn splitter_passes_plain_text_through() {
        let (content, reasoning) = split_parts(&["No tags ", "here at all."]);
        assert_eq!(content, "No tags here at all.");
        assert_eq!(reasoning, "");
    }

    #[test]
    fn splitter_treats_unclosed_think_as_reasoning() {
        let (content, reasoning) = split_parts(&["Before <think>never closed"]);
        assert_eq!(content, "Before ");
        assert_eq!(reasoning, "never closed");
    }

    #[test]
    fn splitter_flushes_false_partial_tag_as_content() {
        // "<th" at end of stream that never became "<think>" is literal text.
        let (content, reasoning) = split_parts(&["a < b and <th"]);
        assert_eq!(content, "a < b and <th");
        assert_eq!(reasoning, "");
    }

    #[test]
    fn splitter_handles_multiple_think_blocks() {
        let (content, reasoning) =
            split_parts(&["<think>one</think>First. <think>two</think>Second."]);
        assert_eq!(content, "First. Second.");
        assert_eq!(reasoning, "onetwo");
    }

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
