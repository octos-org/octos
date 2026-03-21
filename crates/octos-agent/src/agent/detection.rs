//! Detection of repetitive output and retriable responses.

use octos_llm::{ChatResponse, StopReason};

use super::Agent;

impl Agent {
    /// Check if an LLM response is empty or abnormal and should be retried.
    /// Catches:
    /// - Empty content with no tool calls and no reasoning (including output_tokens > 0 bug)
    /// - Content filtered by safety/moderation
    pub(super) fn is_retriable_response(response: &ChatResponse) -> bool {
        let has_reasoning = response
            .reasoning_content
            .as_ref()
            .is_some_and(|r| !r.is_empty());
        let is_empty = response.content.as_ref().is_none_or(|c| c.is_empty())
            && response.tool_calls.is_empty()
            && !has_reasoning;
        let is_filtered = response.stop_reason == StopReason::ContentFiltered;
        is_empty || is_filtered
    }

    /// Detect if text content is stuck in a repetitive loop.
    /// Returns true if the same phrase (>= 20 chars) repeats 5+ times.
    pub(super) fn is_repetitive_output(text: &str) -> bool {
        // Use char count for multi-byte safety (Chinese, emoji, etc.)
        let char_count = text.chars().count();
        if char_count < 200 {
            return false;
        }
        // Check last 500 chars for repeating patterns of 20-100 char lengths
        let check_region: String = if char_count > 500 {
            text.chars().skip(char_count - 500).collect()
        } else {
            text.to_string()
        };
        let region_chars: Vec<char> = check_region.chars().collect();
        let region_len = region_chars.len();
        for pattern_len in [20, 40, 60, 100] {
            if region_len < pattern_len * 3 {
                continue;
            }
            let pattern: String = region_chars[region_len - pattern_len..].iter().collect();
            let count = check_region.matches(&pattern).count();
            if count >= 4 {
                return true;
            }
        }
        false
    }

    /// Check if an error looks like a transient server issue worth retrying.
    pub(super) fn is_retryable_stream_error(err: &eyre::Report) -> bool {
        let msg = err.to_string().to_lowercase();
        msg.contains("overloaded")
            || msg.contains("temporarily")
            || msg.contains("429")
            || msg.contains("502")
            || msg.contains("503")
            || msg.contains("1305")
            || msg.contains("rate limit")
            || msg.contains("decoding response")
            || msg.contains("stream error")
            || msg.contains("connection reset")
            || msg.contains("broken pipe")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_core::ToolCall;
    use octos_llm::{ChatResponse, StopReason, TokenUsage as LlmTokenUsage};

    fn make_response(
        content: Option<&str>,
        tool_calls: Vec<ToolCall>,
        output_tokens: u32,
    ) -> ChatResponse {
        make_response_with_stop(content, tool_calls, output_tokens, StopReason::EndTurn)
    }

    fn make_response_with_stop(
        content: Option<&str>,
        tool_calls: Vec<ToolCall>,
        output_tokens: u32,
        stop_reason: StopReason,
    ) -> ChatResponse {
        ChatResponse {
            content: content.map(String::from),
            reasoning_content: None,
            tool_calls,
            stop_reason,
            usage: LlmTokenUsage {
                input_tokens: 0,
                output_tokens,
                ..Default::default()
            },
        }
    }

    // ---------- Agent::is_retriable_response ----------

    #[test]
    fn should_retry_when_all_empty() {
        let r = make_response(None, vec![], 0);
        assert!(Agent::is_retriable_response(&r));

        let r2 = make_response(Some(""), vec![], 0);
        assert!(Agent::is_retriable_response(&r2));
    }

    #[test]
    fn should_not_retry_with_content() {
        let r = make_response(Some("hello"), vec![], 0);
        assert!(!Agent::is_retriable_response(&r));
    }

    #[test]
    fn should_not_retry_with_tool_calls() {
        let tc = ToolCall {
            id: "1".into(),
            name: "test".into(),
            arguments: serde_json::json!({}),
            metadata: None,
        };
        let r = make_response(None, vec![tc], 0);
        assert!(!Agent::is_retriable_response(&r));
    }

    #[test]
    fn should_retry_with_tokens_but_no_content() {
        let r = make_response(None, vec![], 10);
        assert!(Agent::is_retriable_response(&r));
    }

    #[test]
    fn should_retry_when_content_filtered() {
        let r = make_response_with_stop(None, vec![], 0, StopReason::ContentFiltered);
        assert!(Agent::is_retriable_response(&r));

        // Even with partial content, content_filtered should retry
        let r2 = make_response_with_stop(Some("partial"), vec![], 10, StopReason::ContentFiltered);
        assert!(Agent::is_retriable_response(&r2));
    }

    // ---------- Agent::is_repetitive_output ----------

    #[test]
    fn should_detect_repetitive_output() {
        let repeated = "This is a test phrase. ".repeat(30);
        assert!(Agent::is_repetitive_output(&repeated));
    }

    #[test]
    fn should_not_flag_normal_output() {
        let normal = "The quick brown fox jumps over the lazy dog. \
                      Pack my box with five dozen liquor jugs. \
                      How vexingly quick daft zebras jump.";
        assert!(!Agent::is_repetitive_output(normal));
    }

    #[test]
    fn should_not_flag_short_text() {
        assert!(!Agent::is_repetitive_output("hello hello hello"));
    }

    // ---------- Agent::is_retryable_stream_error ----------

    #[test]
    fn is_retryable_stream_error_transient_errors() {
        for keyword in ["overloaded", "429", "503", "rate limit"] {
            let err = eyre::eyre!("Server error: {}", keyword);
            assert!(
                Agent::is_retryable_stream_error(&err),
                "expected retryable for: {keyword}"
            );
        }
    }

    #[test]
    fn is_retryable_stream_error_non_retryable() {
        let err = eyre::eyre!("invalid json");
        assert!(!Agent::is_retryable_stream_error(&err));
    }
}
