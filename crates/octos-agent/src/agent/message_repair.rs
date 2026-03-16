//! Message normalization, ordering repair, and tool pair validation.

use octos_core::{Message, MessageRole};

/// Sanitize a tool_call_id to contain only characters accepted by all providers.
/// Some models (e.g. Moonshot/kimi) generate IDs like "admin_view_sessions:11"
/// which OpenAI rejects (only allows letters, numbers, underscores, dashes).
pub(crate) fn sanitize_tool_call_id(id: &str) -> String {
    id.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' => c,
            _ => '_',
        })
        .collect()
}

/// Merge all system messages into the first one so providers that require a
/// single leading system message (e.g. Qwen) don't reject the request.
///
/// After context compaction or session history reload, system messages can end
/// up scattered throughout the message list.  This collects their content into
/// the first system message and removes the rest.
pub(crate) fn normalize_system_messages(messages: &mut Vec<Message>) {
    if messages.len() <= 1 {
        return;
    }

    // Convert context-bearing system messages (background task results,
    // conversation summaries) to user messages so they don't bloat the
    // system prompt.  These contain prior conversation content, not
    // instructions for the model.
    for m in messages.iter_mut().skip(1) {
        if m.role == MessageRole::System
            && (m.content.starts_with("[Background task")
                || m.content.starts_with("[Conversation summary]"))
        {
            m.role = MessageRole::User;
            m.content = format!("[System note] {}", m.content);
        }
    }

    // Merge remaining extra system messages (actual instructions) into
    // the first system prompt.
    let mut extra_indices = Vec::new();
    for (i, m) in messages.iter().enumerate().skip(1) {
        if m.role == MessageRole::System {
            extra_indices.push(i);
        }
    }
    if extra_indices.is_empty() {
        return;
    }
    let extra_content: Vec<String> = extra_indices
        .iter()
        .filter_map(|&i| {
            let c = &messages[i].content;
            if c.is_empty() { None } else { Some(c.clone()) }
        })
        .collect();
    if !extra_content.is_empty() {
        let first = &mut messages[0];
        for text in extra_content {
            first.content.push_str("\n\n");
            first.content.push_str(&text);
        }
    }
    for &i in extra_indices.iter().rev() {
        messages.remove(i);
    }
}

/// Gather scattered tool results to be contiguous with their parent assistant.
///
/// OpenAI-compatible APIs require: assistant(tool_calls) -> tool(result)*
/// with no other messages in between.  In speculative/concurrent mode,
/// multiple conversation threads (primary + overflow) save messages to the
/// same session, so tool results may be separated from their parent by
/// user messages, system messages, or other threads' tool_call groups.
///
/// Strategy:
/// 1. For each assistant with tool_calls, extract ALL matching tool results
///    from the entire message list (both before and after the assistant).
/// 2. Deduplicate by tool_call_id (keep the latest result for each ID).
/// 3. Re-insert exactly one result per tool_call right after the assistant.
///
/// This handles backward-stranded results (e.g. from overflow tasks saving
/// results before the assistant message) and duplicate results.
pub(crate) fn repair_message_order(messages: &mut Vec<Message>) {
    use std::collections::{HashMap, HashSet};

    let mut i = 0;
    while i < messages.len() {
        // Find assistant message with tool_calls
        let has_tool_calls = messages[i].role == MessageRole::Assistant
            && messages[i]
                .tool_calls
                .as_ref()
                .is_some_and(|tc| !tc.is_empty());
        if !has_tool_calls {
            i += 1;
            continue;
        }

        // Collect expected tool_call IDs
        let expected_ids: HashSet<String> = messages[i]
            .tool_calls
            .as_ref()
            .unwrap()
            .iter()
            .map(|tc| tc.id.clone())
            .collect();

        // Extract ALL matching tool results from the entire message list.
        // For duplicate tool_call_ids, keep the LAST one (most recent result).
        let mut collected: HashMap<String, Message> = HashMap::new();
        let mut j = 0;
        while j < messages.len() {
            if j == i {
                j += 1;
                continue;
            }
            let is_match = messages[j].role == MessageRole::Tool
                && messages[j]
                    .tool_call_id
                    .as_ref()
                    .is_some_and(|id| expected_ids.contains(id));
            if is_match {
                let msg = messages.remove(j);
                // Overwrite keeps the last occurrence (latest result)
                let id = msg.tool_call_id.clone().unwrap();
                collected.insert(id, msg);
                // Adjust i if we removed before it
                if j < i {
                    i -= 1;
                }
                continue; // don't increment j -- removal shifted elements
            }
            j += 1;
        }

        // Re-insert one result per tool_call right after the assistant,
        // in the same order as tool_calls appear in the assistant message.
        let call_ids: Vec<String> = messages[i]
            .tool_calls
            .as_ref()
            .map(|calls| calls.iter().map(|tc| tc.id.clone()).collect())
            .unwrap_or_default();
        let mut insert_pos = i + 1;
        for id in &call_ids {
            if let Some(msg) = collected.remove(id) {
                messages.insert(insert_pos, msg);
                insert_pos += 1;
            }
        }

        i = insert_pos;
    }
}

/// Repair orphaned tool_call / tool_result pairs in the message list.
///
/// LLM providers reject messages where an assistant has tool_calls but the
/// corresponding tool result messages are missing (or vice versa).  This can
/// happen when compaction or session history truncation splits a tool group.
///
/// Strategy: find matched pairs (call ID exists in both assistant tool_calls
/// AND tool result messages). Strip anything unmatched.
pub(crate) fn repair_tool_pairs(messages: &mut Vec<Message>) {
    use std::collections::HashSet;

    // Collect all tool_call IDs from assistant messages
    let call_ids: HashSet<String> = messages
        .iter()
        .filter(|m| m.role == MessageRole::Assistant)
        .flat_map(|m| {
            m.tool_calls
                .as_ref()
                .into_iter()
                .flat_map(|calls| calls.iter().map(|tc| tc.id.clone()))
        })
        .collect();

    // Collect all tool_call_ids from Tool result messages
    let result_ids: HashSet<String> = messages
        .iter()
        .filter(|m| m.role == MessageRole::Tool)
        .filter_map(|m| m.tool_call_id.clone())
        .collect();

    // Matched = present in both sets
    let matched: HashSet<&String> = call_ids.intersection(&result_ids).collect();

    // Strip tool_calls from assistant messages where ANY call ID is unmatched
    for m in messages.iter_mut() {
        if m.role == MessageRole::Assistant {
            if let Some(ref calls) = m.tool_calls {
                if calls.iter().any(|tc| !matched.contains(&tc.id)) {
                    let names: Vec<_> = calls.iter().map(|tc| tc.name.as_str()).collect();
                    if m.content.is_empty() {
                        m.content = format!("[Called tools: {}]", names.join(", "));
                    }
                    m.tool_calls = None;
                }
            }
        }
    }

    // Remove Tool result messages whose call ID is unmatched or whose
    // parent assistant had its tool_calls stripped.
    let remaining_call_ids: HashSet<String> = messages
        .iter()
        .filter(|m| m.role == MessageRole::Assistant)
        .flat_map(|m| {
            m.tool_calls
                .as_ref()
                .into_iter()
                .flat_map(|calls| calls.iter().map(|tc| tc.id.clone()))
        })
        .collect();

    messages.retain(|m| {
        if m.role == MessageRole::Tool {
            match m.tool_call_id {
                Some(ref id) => return remaining_call_ids.contains(id),
                None => return false, // Tool messages without tool_call_id are invalid
            }
        }
        true
    });
}

/// Ensure every assistant message with tool_calls has a matching tool result
/// for EACH tool_call_id.  If any result is missing, synthesize a placeholder
/// so LLM providers don't reject the request with 400 Bad Request.
///
/// This is a last-resort safety net that runs after repair_message_order and
/// repair_tool_pairs.  It handles edge cases where tool results were lost
/// (e.g. session write failure, crash between assistant save and tool result
/// save, or ID mismatch after sanitization).
pub(crate) fn synthesize_missing_tool_results(messages: &mut Vec<Message>) {
    use std::collections::HashSet;

    let mut i = 0;
    while i < messages.len() {
        let has_tool_calls = messages[i].role == MessageRole::Assistant
            && messages[i]
                .tool_calls
                .as_ref()
                .is_some_and(|tc| !tc.is_empty());
        if !has_tool_calls {
            i += 1;
            continue;
        }

        let call_ids: Vec<(String, String)> = messages[i]
            .tool_calls
            .as_ref()
            .unwrap()
            .iter()
            .map(|tc| (tc.id.clone(), tc.name.clone()))
            .collect();

        // Collect existing tool result IDs in the window after this assistant
        let mut existing: HashSet<String> = HashSet::new();
        let mut j = i + 1;
        while j < messages.len() {
            if messages[j].role == MessageRole::Tool {
                if let Some(ref id) = messages[j].tool_call_id {
                    existing.insert(id.clone());
                }
            } else if messages[j].role == MessageRole::Assistant
                || messages[j].role == MessageRole::User
            {
                break; // stop at next non-tool message
            }
            j += 1;
        }

        // Synthesize placeholders for missing results
        let insert_pos = j; // insert after existing tool results
        let mut inserted = 0;
        for (id, name) in &call_ids {
            if !existing.contains(id) {
                tracing::warn!(
                    tool_call_id = %id,
                    tool_name = %name,
                    "synthesizing missing tool result to prevent provider 400 error"
                );
                messages.insert(
                    insert_pos + inserted,
                    Message {
                        role: MessageRole::Tool,
                        content: format!("[Tool '{}' result was lost — no output available]", name),
                        media: vec![],
                        tool_calls: None,
                        tool_call_id: Some(id.clone()),
                        reasoning_content: None,
                        timestamp: messages[i].timestamp,
                    },
                );
                inserted += 1;
            }
        }

        i = insert_pos + inserted;
    }
}

/// Truncate long tool result messages from prior conversation rounds.
///
/// When a session contains multi-round conversations, old tool results
/// (e.g. a 10,000-word research report from `run_pipeline`) dominate the
/// context window and cause the LLM to re-engage with prior questions
/// instead of focusing on the latest user message.
///
/// This function finds the last user message (the current question) and
/// truncates tool result messages that appear BEFORE it if they exceed
/// `MAX_OLD_TOOL_RESULT_CHARS`.  Tool results in the current conversation
/// round (after the last user message) are kept intact so the agent can
/// reference them.
pub(crate) fn truncate_old_tool_results(messages: &mut [Message]) {
    const MAX_OLD_TOOL_RESULT_CHARS: usize = 800;

    // Find the last user message -- everything before it is "old" context
    let last_user_idx = messages.iter().rposition(|m| m.role == MessageRole::User);
    let boundary = match last_user_idx {
        Some(idx) => idx,
        None => return, // no user message, nothing to truncate
    };

    for msg in messages[..boundary].iter_mut() {
        if msg.role == MessageRole::Tool && msg.content.len() > MAX_OLD_TOOL_RESULT_CHARS {
            let truncated: String = msg
                .content
                .chars()
                .take(MAX_OLD_TOOL_RESULT_CHARS)
                .collect();
            msg.content = format!("{truncated}\n\n[... truncated for brevity]");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sys(content: &str) -> Message {
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

    fn assistant_with_tools(tool_ids: &[&str]) -> Message {
        Message {
            role: MessageRole::Assistant,
            content: String::new(),
            media: vec![],
            tool_calls: Some(
                tool_ids
                    .iter()
                    .map(|id| octos_core::ToolCall {
                        id: id.to_string(),
                        name: "test_tool".to_string(),
                        arguments: serde_json::json!({}),
                        metadata: None,
                    })
                    .collect(),
            ),
            tool_call_id: None,
            reasoning_content: None,
            timestamp: chrono::Utc::now(),
        }
    }

    fn tool_result_msg(id: &str) -> Message {
        Message {
            role: MessageRole::Tool,
            content: "result".to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: Some(id.to_string()),
            reasoning_content: None,
            timestamp: chrono::Utc::now(),
        }
    }

    // ---------- normalize_system_messages ----------

    #[test]
    fn should_merge_multiple_system_messages_into_first() {
        let mut msgs = vec![
            sys("system prompt"),
            sys("compaction summary"),
            user("hello"),
        ];
        normalize_system_messages(&mut msgs);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, MessageRole::System);
        assert!(msgs[0].content.contains("system prompt"));
        assert!(msgs[0].content.contains("compaction summary"));
        assert_eq!(msgs[1].role, MessageRole::User);
    }

    #[test]
    fn should_merge_scattered_system_messages() {
        let mut msgs = vec![
            sys("prompt"),
            user("msg1"),
            sys("mid-summary"),
            user("msg2"),
        ];
        normalize_system_messages(&mut msgs);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].role, MessageRole::System);
        assert!(msgs[0].content.contains("prompt"));
        assert!(msgs[0].content.contains("mid-summary"));
        assert_eq!(msgs[1].role, MessageRole::User);
        assert_eq!(msgs[2].role, MessageRole::User);
    }

    #[test]
    fn should_noop_when_single_system_message() {
        let mut msgs = vec![sys("prompt"), user("hello")];
        normalize_system_messages(&mut msgs);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].content, "prompt");
    }

    // ---------- repair_tool_pairs ----------

    #[test]
    fn should_strip_orphaned_tool_calls() {
        let mut msgs = vec![
            sys("prompt"),
            assistant_with_tools(&["tc1", "tc2"]),
            tool_result_msg("tc1"),
            // tc2 result is missing -- orphaned
            user("next question"),
        ];
        repair_tool_pairs(&mut msgs);
        // assistant's tool_calls should be stripped (tc2 has no result)
        assert!(msgs[1].tool_calls.is_none());
        assert!(msgs[1].content.contains("test_tool"));
        // tc1 result should also be removed (its assistant lost tool_calls)
        assert_eq!(msgs.len(), 3); // sys, assistant(text), user
    }

    #[test]
    fn should_keep_complete_tool_pairs() {
        let mut msgs = vec![
            sys("prompt"),
            assistant_with_tools(&["tc1"]),
            tool_result_msg("tc1"),
            user("thanks"),
        ];
        repair_tool_pairs(&mut msgs);
        assert_eq!(msgs.len(), 4);
        assert!(msgs[1].tool_calls.is_some());
    }

    #[test]
    fn should_remove_orphaned_tool_results() {
        let mut msgs = vec![
            sys("prompt"),
            tool_result_msg("tc_orphan"), // no matching assistant
            user("hello"),
        ];
        repair_tool_pairs(&mut msgs);
        assert_eq!(msgs.len(), 2); // sys, user
    }

    // ---------- repair_message_order ----------

    #[test]
    fn should_gather_scattered_tool_result_past_user_message() {
        let mut msgs = vec![
            sys("prompt"),
            assistant_with_tools(&["tc1"]),
            user("new question"),   // overflow user msg
            tool_result_msg("tc1"), // scattered result
        ];
        repair_message_order(&mut msgs);
        assert_eq!(msgs[0].role, MessageRole::System);
        assert_eq!(msgs[1].role, MessageRole::Assistant);
        assert_eq!(msgs[2].role, MessageRole::Tool);
        assert_eq!(msgs[2].tool_call_id.as_deref(), Some("tc1"));
        assert_eq!(msgs[3].role, MessageRole::User);
        assert_eq!(msgs[3].content, "new question");
    }

    #[test]
    fn should_gather_scattered_tool_results_past_system_message() {
        let mut msgs = vec![
            assistant_with_tools(&["tc1", "tc2"]),
            tool_result_msg("tc1"),
            sys("background task result"),
            tool_result_msg("tc2"),
        ];
        repair_message_order(&mut msgs);
        assert_eq!(msgs[0].role, MessageRole::Assistant);
        assert_eq!(msgs[1].role, MessageRole::Tool);
        assert_eq!(msgs[1].tool_call_id.as_deref(), Some("tc1"));
        assert_eq!(msgs[2].role, MessageRole::Tool);
        assert_eq!(msgs[2].tool_call_id.as_deref(), Some("tc2"));
        assert_eq!(msgs[3].role, MessageRole::System);
    }

    #[test]
    fn should_handle_concurrent_tool_call_threads() {
        let mut msgs = vec![
            user("make slides"),
            assistant_with_tools(&["tc1"]),
            user("what time is it"),
            assistant_with_tools(&["tc2"]),
            tool_result_msg("tc2"),
            tool_result_msg("tc1"),
        ];
        repair_message_order(&mut msgs);
        assert_eq!(msgs[0].role, MessageRole::User);
        assert_eq!(msgs[1].role, MessageRole::Assistant);
        assert_eq!(msgs[2].role, MessageRole::Tool);
        assert_eq!(msgs[2].tool_call_id.as_deref(), Some("tc1"));
        assert_eq!(msgs[3].role, MessageRole::User);
        assert_eq!(msgs[4].role, MessageRole::Assistant);
        assert_eq!(msgs[5].role, MessageRole::Tool);
        assert_eq!(msgs[5].tool_call_id.as_deref(), Some("tc2"));
    }

    #[test]
    fn should_not_modify_valid_message_order() {
        let mut msgs = vec![
            sys("prompt"),
            assistant_with_tools(&["tc1"]),
            tool_result_msg("tc1"),
            user("thanks"),
        ];
        let original_len = msgs.len();
        repair_message_order(&mut msgs);
        assert_eq!(msgs.len(), original_len);
        assert_eq!(msgs[3].content, "thanks");
    }

    #[test]
    fn should_gather_backward_stranded_tool_result() {
        let mut msgs = vec![
            sys("prompt"),
            user("tts"),
            tool_result_msg("tc1"),
            assistant_with_tools(&["tc1"]),
            tool_result_msg("tc1"),
            user("next question"),
        ];
        repair_message_order(&mut msgs);
        assert_eq!(msgs[0].role, MessageRole::System);
        assert_eq!(msgs[1].role, MessageRole::User);
        assert_eq!(msgs[1].content, "tts");
        assert_eq!(msgs[2].role, MessageRole::Assistant);
        assert_eq!(msgs[3].role, MessageRole::Tool);
        assert_eq!(msgs[3].tool_call_id.as_deref(), Some("tc1"));
        assert_eq!(msgs[4].role, MessageRole::User);
        assert_eq!(msgs[4].content, "next question");
        assert_eq!(msgs.len(), 5);
    }

    #[test]
    fn should_remove_tool_result_with_no_tool_call_id() {
        let mut msgs = vec![
            sys("prompt"),
            assistant_with_tools(&["tc1"]),
            tool_result_msg("tc1"),
            Message {
                role: MessageRole::Tool,
                content: "Tool task panicked".to_string(),
                media: vec![],
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                timestamp: chrono::Utc::now(),
            },
            user("thanks"),
        ];
        repair_tool_pairs(&mut msgs);
        assert_eq!(msgs.len(), 4); // sys, assistant, tool(tc1), user
    }

    // ---------- sanitize_tool_call_id ----------

    #[test]
    fn should_sanitize_colons_in_tool_call_id() {
        assert_eq!(
            sanitize_tool_call_id("admin_view_sessions:11"),
            "admin_view_sessions_11"
        );
    }

    #[test]
    fn should_preserve_valid_tool_call_id() {
        assert_eq!(sanitize_tool_call_id("call_0_shell"), "call_0_shell");
        assert_eq!(sanitize_tool_call_id("toolu_01A-bC"), "toolu_01A-bC");
    }

    #[test]
    fn should_sanitize_special_chars_in_tool_call_id() {
        assert_eq!(
            sanitize_tool_call_id("id.with.dots:and:colons"),
            "id_with_dots_and_colons"
        );
    }

    // ---------- synthesize_missing_tool_results ----------

    #[test]
    fn should_synthesize_missing_tool_results() {
        let mut msgs = vec![
            sys("prompt"),
            assistant_with_tools(&["tc1", "tc2", "tc3"]),
            tool_result_msg("tc1"),
            // tc2 and tc3 results are missing
            user("next"),
        ];
        synthesize_missing_tool_results(&mut msgs);
        // Should have 6 messages: sys, assistant, tc1 result, tc2 synth, tc3 synth, user
        assert_eq!(msgs.len(), 6);
        assert_eq!(msgs[2].tool_call_id.as_deref(), Some("tc1"));
        assert_eq!(msgs[2].content, "result"); // original
        assert_eq!(msgs[3].tool_call_id.as_deref(), Some("tc2"));
        assert!(msgs[3].content.contains("lost"));
        assert_eq!(msgs[4].tool_call_id.as_deref(), Some("tc3"));
        assert!(msgs[4].content.contains("lost"));
        assert_eq!(msgs[5].role, MessageRole::User);
    }

    #[test]
    fn should_not_synthesize_when_all_results_present() {
        let mut msgs = vec![
            assistant_with_tools(&["tc1", "tc2"]),
            tool_result_msg("tc1"),
            tool_result_msg("tc2"),
            user("thanks"),
        ];
        let original_len = msgs.len();
        synthesize_missing_tool_results(&mut msgs);
        assert_eq!(msgs.len(), original_len);
    }

    #[test]
    fn should_synthesize_all_missing_when_no_results_exist() {
        let mut msgs = vec![assistant_with_tools(&["tc1", "tc2"]), user("next")];
        synthesize_missing_tool_results(&mut msgs);
        assert_eq!(msgs.len(), 4); // assistant, tc1 synth, tc2 synth, user
        assert_eq!(msgs[1].tool_call_id.as_deref(), Some("tc1"));
        assert_eq!(msgs[2].tool_call_id.as_deref(), Some("tc2"));
        assert_eq!(msgs[3].role, MessageRole::User);
    }
}
