//! Extractive context compaction for fitting conversation history into context windows.
//!
//! Replaces simple message truncation with intelligent summarization:
//! old messages are compressed into a "Conversation Summary" system message
//! while recent messages are kept verbatim.

use octos_core::{Message, MessageRole};
use octos_llm::context::{estimate_message_tokens, estimate_tokens};

/// Safety margin multiplier for token estimation inaccuracy.
pub(crate) const SAFETY_MARGIN: f64 = 1.2;

/// Minimum non-system messages to always keep intact (recent context).
pub(crate) const MIN_RECENT_MESSAGES: usize = 6;

/// Target compression ratio for summarized content.
const BASE_CHUNK_RATIO: f64 = 0.4;

/// Find the boundary between old (compactable) and recent (kept verbatim) messages.
///
/// Returns the index where the recent zone starts. Messages `[1..split]` are old,
/// `[split..]` are recent. Never splits inside an assistant-tool pair.
pub(crate) fn find_recent_boundary(messages: &[Message], budget: u32, system_tokens: u32) -> usize {
    let mut recent_tokens = 0u32;
    let mut count = 0usize;
    let mut split = messages.len();

    for i in (1..messages.len()).rev() {
        let msg_tokens = estimate_message_tokens(&messages[i]);
        count += 1;

        if count >= MIN_RECENT_MESSAGES && system_tokens + recent_tokens + msg_tokens > budget / 2 {
            break;
        }

        recent_tokens += msg_tokens;
        split = i;
    }

    // Don't split inside a tool-call group: if split points to a Tool message,
    // walk back past all consecutive Tool messages and the preceding Assistant
    // message (which may have multiple parallel tool_calls).
    while split > 1 && messages[split].role == MessageRole::Tool {
        split -= 1;
    }

    split
}

/// Build an extractive summary of old messages within a token budget.
///
/// Extracts first lines from each message, strips tool call arguments
/// (security: untrusted payloads), and drops media references.
pub(crate) fn compact_messages(messages: &[Message], budget_tokens: u32) -> String {
    let mut lines = Vec::new();
    let header = format!(
        "## Conversation Summary (compacted from {} messages)\n",
        messages.len()
    );
    let mut running_tokens = estimate_tokens(&header);
    lines.push(header);

    let target = (budget_tokens as f64 * BASE_CHUNK_RATIO) as u32;

    for (i, msg) in messages.iter().enumerate() {
        if running_tokens >= target {
            lines.push(format!(
                "... ({} earlier messages omitted)",
                messages.len() - i
            ));
            break;
        }

        let line = summarize_message(msg, messages);
        let line_tokens = estimate_tokens(&line);

        if running_tokens + line_tokens > budget_tokens {
            lines.push(format!(
                "... ({} earlier messages omitted)",
                messages.len() - i
            ));
            break;
        }

        running_tokens += line_tokens;
        lines.push(line);
    }

    lines.join("\n")
}

/// Summarize a single message into a compact text line.
fn summarize_message(msg: &Message, context: &[Message]) -> String {
    match msg.role {
        MessageRole::User => {
            let media_note = if msg.media.is_empty() {
                ""
            } else {
                " [media omitted]"
            };
            format!("> User: {}{}", first_line(&msg.content, 200), media_note)
        }
        MessageRole::Assistant => {
            let mut parts = Vec::new();
            if let Some(ref calls) = msg.tool_calls {
                for call in calls {
                    parts.push(format!("- Called {}", call.name));
                }
            }
            if !msg.content.is_empty() {
                let prefix = if msg.tool_calls.is_some() {
                    "  "
                } else {
                    "> Assistant: "
                };
                parts.push(format!("{}{}", prefix, first_line(&msg.content, 200)));
            }
            parts.join("\n")
        }
        MessageRole::Tool => {
            let tool_name = find_tool_name(msg, context);
            let status = if msg.content.starts_with("Error:") {
                "error"
            } else {
                "ok"
            };
            format!(
                "  -> {}: {} - {}",
                tool_name,
                status,
                first_line(&msg.content, 100)
            )
        }
        MessageRole::System => {
            format!("> Context: {}", first_line(&msg.content, 200))
        }
    }
}

/// Extract the first line of text, truncated to max_chars (UTF-8 safe).
fn first_line(s: &str, max_chars: usize) -> String {
    let line = s.lines().next().unwrap_or("");
    if line.chars().count() <= max_chars {
        line.to_string()
    } else {
        let end = line
            .char_indices()
            .nth(max_chars)
            .map(|(i, _)| i)
            .unwrap_or(line.len());
        format!("{}...", &line[..end])
    }
}

/// Resolve a tool message's tool_call_id to the tool name from context.
fn find_tool_name(tool_msg: &Message, messages: &[Message]) -> String {
    if let Some(ref target_id) = tool_msg.tool_call_id {
        for msg in messages.iter().rev() {
            if let Some(ref calls) = msg.tool_calls {
                for call in calls {
                    if call.id == *target_id {
                        return call.name.clone();
                    }
                }
            }
        }
    }
    "unknown_tool".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_core::ToolCall;

    fn user_msg(content: &str) -> Message {
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

    fn assistant_msg(content: &str) -> Message {
        Message {
            role: MessageRole::Assistant,
            content: content.to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
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
                arguments: serde_json::json!({"path": "/secret/file", "content": "x".repeat(1000)}),
                metadata: None,
            }]),
            tool_call_id: None,
            reasoning_content: None,
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
            timestamp: chrono::Utc::now(),
        }
    }

    fn system_msg(content: &str) -> Message {
        Message {
            role: MessageRole::System,
            content: content.to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            timestamp: chrono::Utc::now(),
        }
    }

    #[test]
    fn test_compact_messages_basic() {
        let messages = vec![
            user_msg("Hello, can you help me?"),
            assistant_msg("Sure, I can help!"),
            user_msg("Read the file"),
            assistant_tool_call("read_file", "tc1"),
            tool_result("tc1", "fn main() { println!(\"hello\"); }"),
            assistant_msg("Here is the file content."),
        ];

        let summary = compact_messages(&messages, 10000);
        assert!(summary.contains("Conversation Summary"));
        assert!(summary.contains("> User: Hello"));
        assert!(summary.contains("> Assistant: Sure"));
        assert!(summary.contains("Called read_file"));
        assert!(summary.contains("-> read_file: ok"));
    }

    #[test]
    fn test_compact_strips_tool_arguments() {
        let messages = vec![
            assistant_tool_call("write_file", "tc1"),
            tool_result("tc1", "File written."),
        ];

        let summary = compact_messages(&messages, 10000);
        // Should contain tool name but NOT the argument content
        assert!(summary.contains("Called write_file"));
        assert!(!summary.contains("/secret/file"));
        assert!(!summary.contains("xxxx"));
    }

    #[test]
    fn test_compact_budget_enforcement() {
        // Create many messages to exceed a small budget
        let mut messages = Vec::new();
        for i in 0..50 {
            messages.push(user_msg(&format!("Message number {} with some content", i)));
            messages.push(assistant_msg(&format!("Response number {} here", i)));
        }

        let summary = compact_messages(&messages, 200);
        let summary_tokens = estimate_tokens(&summary);
        assert!(summary_tokens <= 200);
        assert!(summary.contains("earlier messages omitted"));
    }

    #[test]
    fn test_compact_media_omitted() {
        let messages = vec![Message {
            role: MessageRole::User,
            content: "Look at this image".to_string(),
            media: vec!["photo.jpg".to_string()],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            timestamp: chrono::Utc::now(),
        }];

        let summary = compact_messages(&messages, 10000);
        assert!(summary.contains("[media omitted]"));
        assert!(!summary.contains("photo.jpg"));
    }

    #[test]
    fn test_compact_error_tool_result() {
        let messages = vec![
            assistant_tool_call("shell", "tc1"),
            tool_result("tc1", "Error: command not found"),
        ];

        let summary = compact_messages(&messages, 10000);
        assert!(summary.contains("-> shell: error"));
    }

    #[test]
    fn test_find_recent_boundary_tool_pairing() {
        // Build enough messages so that a tight budget forces compaction (split > 1).
        // system + 10 user/assistant pairs + tool pair in the middle
        let mut messages = vec![system_msg("system prompt")];
        for i in 0..5 {
            messages.push(user_msg(&format!(
                "question {} with enough text to use tokens",
                i
            )));
            messages.push(assistant_msg(&format!(
                "answer {} with enough text to use tokens",
                i
            )));
        }
        // Insert a tool call pair
        messages.push(assistant_tool_call("read_file", "tc1"));
        messages.push(tool_result("tc1", "file content here"));
        for i in 5..10 {
            messages.push(user_msg(&format!(
                "question {} with enough text to use tokens",
                i
            )));
            messages.push(assistant_msg(&format!(
                "answer {} with enough text to use tokens",
                i
            )));
        }

        // Use a small budget so split is forced past index 1
        let split = find_recent_boundary(&messages, 200, 50);
        assert!(split > 1, "budget should force compaction, split={split}");
        // split should not land on a Tool message
        assert_ne!(messages[split].role, MessageRole::Tool);
    }

    #[test]
    fn test_first_line_utf8_safe() {
        let text = "Hello world";
        assert_eq!(first_line(text, 5), "Hello...");

        let cjk = "你好世界测试文本";
        assert_eq!(first_line(cjk, 4), "你好世界...");

        let short = "hi";
        assert_eq!(first_line(short, 100), "hi");
    }

    #[test]
    fn test_find_tool_name_resolves() {
        let messages = vec![
            assistant_tool_call("grep", "tc1"),
            tool_result("tc1", "found matches"),
        ];
        let name = find_tool_name(&messages[1], &messages);
        assert_eq!(name, "grep");
    }

    #[test]
    fn test_find_tool_name_unknown_fallback() {
        let msg = tool_result("nonexistent", "data");
        let name = find_tool_name(&msg, &[]);
        assert_eq!(name, "unknown_tool");
    }

    #[test]
    fn test_summarize_user_message() {
        let msg = user_msg("Hello world");
        let summary = summarize_message(&msg, &[]);
        assert_eq!(summary, "> User: Hello world");
    }

    #[test]
    fn test_summarize_user_message_with_media() {
        let msg = Message {
            role: MessageRole::User,
            content: "Check this".to_string(),
            media: vec!["img.png".to_string()],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            timestamp: chrono::Utc::now(),
        };
        let summary = summarize_message(&msg, &[]);
        assert!(summary.contains("[media omitted]"));
        assert!(summary.contains("Check this"));
    }

    #[test]
    fn test_summarize_assistant_text() {
        let msg = assistant_msg("Here is your answer");
        let summary = summarize_message(&msg, &[]);
        assert_eq!(summary, "> Assistant: Here is your answer");
    }

    #[test]
    fn test_summarize_assistant_tool_call() {
        let msg = assistant_tool_call("read_file", "tc1");
        let summary = summarize_message(&msg, &[]);
        assert!(summary.contains("Called read_file"));
    }

    #[test]
    fn test_summarize_tool_result_ok() {
        let context = vec![assistant_tool_call("grep", "tc1")];
        let msg = tool_result("tc1", "found 3 matches");
        let summary = summarize_message(&msg, &context);
        assert!(summary.contains("-> grep: ok"));
    }

    #[test]
    fn test_summarize_tool_result_error() {
        let context = vec![assistant_tool_call("shell", "tc1")];
        let msg = tool_result("tc1", "Error: command not found");
        let summary = summarize_message(&msg, &context);
        assert!(summary.contains("-> shell: error"));
    }

    #[test]
    fn test_summarize_system_message() {
        let msg = system_msg("You are a coding assistant");
        let summary = summarize_message(&msg, &[]);
        assert_eq!(summary, "> Context: You are a coding assistant");
    }

    #[test]
    fn test_first_line_multiline() {
        let text = "first line\nsecond line\nthird line";
        assert_eq!(first_line(text, 200), "first line");
    }

    #[test]
    fn test_first_line_empty() {
        assert_eq!(first_line("", 200), "");
    }
}
