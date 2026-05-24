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

/// Normalize all tool_call_ids in messages to `call_` prefix.
///
/// When adaptive routing switches providers mid-conversation, the history
/// contains IDs from different providers (toolu_xxx from Anthropic,
/// call_function_xxx from Moonshot, fc_xxx from OpenAI Responses, etc.).
/// OpenAI's APIs reject non-`call_` prefixed IDs with 400 invalid_value.
///
/// This rewrites ALL tool_call_ids to a consistent format, ensuring both
/// the assistant message's tool_calls[].id and the tool message's
/// tool_call_id match.
pub(crate) fn normalize_tool_call_ids(messages: &mut [Message]) -> bool {
    use std::collections::HashMap;

    // Build a mapping of old_id → normalized_id
    let mut id_map: HashMap<String, String> = HashMap::new();

    // First pass: collect all tool_call IDs from assistant messages
    for msg in messages.iter() {
        if let Some(ref tool_calls) = msg.tool_calls {
            for tc in tool_calls {
                if !tc.id.is_empty() && !tc.id.starts_with("call_") {
                    let normalized = normalize_one_id(&tc.id);
                    id_map.insert(tc.id.clone(), normalized);
                }
            }
        }
    }

    if id_map.is_empty() {
        return false;
    }

    // Second pass: rewrite IDs in both assistant tool_calls and tool messages
    let mut changed = false;
    for msg in messages.iter_mut() {
        if let Some(ref mut tool_calls) = msg.tool_calls {
            for tc in tool_calls.iter_mut() {
                if let Some(new_id) = id_map.get(&tc.id) {
                    if tc.id != *new_id {
                        changed = true;
                    }
                    tc.id = new_id.clone();
                }
            }
        }
        if let Some(ref old_id) = msg.tool_call_id {
            if let Some(new_id) = id_map.get(old_id) {
                if old_id != new_id {
                    changed = true;
                }
                msg.tool_call_id = Some(new_id.clone());
            }
        }
    }
    changed
}

fn normalize_one_id(id: &str) -> String {
    if id.starts_with("call_") || id.starts_with("fc_") {
        return id.to_string();
    }
    let stripped = id
        .strip_prefix("call_function_")
        .or_else(|| id.strip_prefix("toolu_"))
        .or_else(|| id.strip_prefix("chatcmpl-"))
        .unwrap_or(id);
    format!("call_{stripped}")
}

/// Merge all system messages into the first one so providers that require a
/// single leading system message (e.g. Qwen) don't reject the request.
///
/// After context compaction or session history reload, system messages can end
/// up scattered throughout the message list.  This collects their content into
/// the first system message and removes the rest.
pub(crate) fn normalize_system_messages(messages: &mut Vec<Message>) -> bool {
    if messages.len() <= 1 {
        return false;
    }

    // Convert context-bearing system messages (background task results,
    // conversation summaries) to user messages so they don't bloat the
    // system prompt.  These contain prior conversation content, not
    // instructions for the model.
    let mut changed = false;
    for m in messages.iter_mut().skip(1) {
        if m.role == MessageRole::System
            && (m.content.starts_with("[Background task")
                || m.content.starts_with("[Conversation summary]"))
        {
            m.role = MessageRole::User;
            m.content = format!("[System note] {}", m.content);
            changed = true;
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
        return changed;
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
        changed = true;
    }
    for &i in extra_indices.iter().rev() {
        messages.remove(i);
        changed = true;
    }
    changed
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
pub(crate) fn repair_message_order(messages: &mut Vec<Message>) -> bool {
    use std::collections::{HashMap, HashSet};

    let mut i = 0;
    let mut changed = false;
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
                changed = true;
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
                changed = true;
                insert_pos += 1;
            }
        }

        i = insert_pos;
    }
    changed
}

/// Repair orphaned tool_call / tool_result pairs in the message list.
///
/// LLM providers reject messages where an assistant has tool_calls but the
/// corresponding tool result messages are missing (or vice versa).  This can
/// happen when compaction or session history truncation splits a tool group.
///
/// Strategy: find matched pairs (call ID exists in both assistant tool_calls
/// AND tool result messages). Strip anything unmatched.
pub(crate) fn repair_tool_pairs(messages: &mut Vec<Message>) -> bool {
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
    let mut changed = false;
    for m in messages.iter_mut() {
        if m.role == MessageRole::Assistant {
            if let Some(ref calls) = m.tool_calls {
                if calls.iter().any(|tc| !matched.contains(&tc.id)) {
                    let names: Vec<_> = calls.iter().map(|tc| tc.name.as_str()).collect();
                    if m.content.is_empty() {
                        m.content = format!("[Called tools: {}]", names.join(", "));
                    }
                    m.tool_calls = None;
                    changed = true;
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

    let original_len = messages.len();
    messages.retain(|m| {
        if m.role == MessageRole::Tool {
            match m.tool_call_id {
                Some(ref id) => return remaining_call_ids.contains(id),
                None => return false, // Tool messages without tool_call_id are invalid
            }
        }
        true
    });
    changed || messages.len() != original_len
}

/// Ensure every assistant message with tool_calls has a matching tool result
/// for EACH tool_call_id.  If any result is missing, synthesize a placeholder
/// so LLM providers don't reject the request with 400 Bad Request.
///
/// This is a last-resort safety net that runs after repair_message_order and
/// repair_tool_pairs.  It handles edge cases where tool results were lost
/// (e.g. session write failure, crash between assistant save and tool result
/// save, or ID mismatch after sanitization).
///
/// NEW-11 (orphan recovery loop): the existence check pre-collects EVERY
/// `MessageRole::Tool` `tool_call_id` in the message list before iterating
/// assistant rows. The original implementation only inspected the window
/// immediately after each assistant message and would re-fabricate when
/// `repair_message_order` failed to gather a result adjacent to its parent
/// (e.g. cascade-fail timeout envelopes for a spawn_only `run_pipeline`
/// that landed on the wire as Assistant rows rather than Tool rows but
/// whose handle-ack Tool row was preserved by `repair_message_order`,
/// leaving a structurally-resolved id that the windowed scan kept treating
/// as "still missing"). Re-fabrication on every iteration was the
/// observed driver of the per-turn `synthesizing missing tool result`
/// WARN that fleet-UX soak round-9 surfaced on mini3
/// (`web-1779655648282-3lj4a4`). Scanning globally for ANY persisted
/// tool result (synthetic ack, success, or failure) closes the loop:
/// once cascade-fail or the spawn_only handle has stamped a row for the
/// id, subsequent passes silently skip it instead of inserting a fresh
/// `[result was lost]` placeholder. The earlier
/// `repair_message_order`/`repair_tool_pairs` passes still re-order or
/// strip orphans where appropriate; this helper is now strictly
/// idempotent over the input.
pub(crate) fn synthesize_missing_tool_results(messages: &mut Vec<Message>) -> bool {
    use std::collections::HashSet;

    // NEW-11: pre-collect every persisted tool_call_id (success OR failure
    // envelope, including the spawn_only handle Tool row inserted by the
    // execution loop) so the per-assistant scan never re-fabricates a
    // placeholder for an id that is already resolved somewhere in the
    // transcript. The set is captured BEFORE we start mutating `messages`,
    // and we add freshly-inserted placeholder ids to it as we go so a
    // single pass cannot double-fabricate for the same id within one
    // assistant's tool_calls list either.
    let mut existing_globally: HashSet<String> = messages
        .iter()
        .filter(|m| m.role == MessageRole::Tool)
        .filter_map(|m| m.tool_call_id.clone())
        .collect();

    let mut i = 0;
    let mut changed = false;
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

        // Walk forward to locate the contiguous block of Tool rows that
        // already follow this assistant. Placeholders for ids NOT yet
        // resolved anywhere in the transcript are inserted at the tail
        // of that block so the assistant -> tool result pairing stays
        // adjacent (providers reject non-contiguous pairs).
        let mut j = i + 1;
        while j < messages.len() {
            if messages[j].role == MessageRole::Tool {
                j += 1;
                continue;
            }
            break;
        }

        let insert_pos = j;
        let mut inserted = 0;
        for (id, name) in &call_ids {
            if !existing_globally.contains(id) {
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
                        client_message_id: None,
                        thread_id: None,
                        timestamp: messages[i].timestamp,
                    },
                );
                // Track the id we just resolved so a later assistant
                // referencing the same id (idempotent reload after a
                // truncated transcript pass) does not re-synthesise.
                existing_globally.insert(id.clone());
                changed = true;
                inserted += 1;
            }
        }

        i = insert_pos + inserted;
    }
    changed
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
pub(crate) fn truncate_old_tool_results(messages: &mut [Message]) -> bool {
    const MAX_OLD_TOOL_RESULT_CHARS: usize = 800;

    // Find the last user message -- everything before it is "old" context
    let last_user_idx = messages.iter().rposition(|m| m.role == MessageRole::User);
    let boundary = match last_user_idx {
        Some(idx) => idx,
        None => return false, // no user message, nothing to truncate
    };

    let mut changed = false;
    for msg in messages[..boundary].iter_mut() {
        if msg.role == MessageRole::Tool && msg.content.len() > MAX_OLD_TOOL_RESULT_CHARS {
            let truncated: String = msg
                .content
                .chars()
                .take(MAX_OLD_TOOL_RESULT_CHARS)
                .collect();
            msg.content = format!("{truncated}\n\n[... truncated for brevity]");
            changed = true;
        }
    }
    changed
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
            client_message_id: None,
            thread_id: None,
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
            client_message_id: None,
            thread_id: None,
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
            client_message_id: None,
            thread_id: None,
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
            client_message_id: None,
            thread_id: None,
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
                client_message_id: None,
                thread_id: None,
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

    /// NEW-11 regression: when a spawn_only `run_pipeline` invocation
    /// kicks off in iteration N and the cascade-fail timeout envelope
    /// lands as a non-Tool assistant row AFTER the user's next message,
    /// the windowed scan in the legacy `synthesize_missing_tool_results`
    /// would still treat the spawn_only handle Tool row (sitting
    /// adjacent to the assistant) as "missing" if `repair_message_order`
    /// hadn't re-gathered it on a subsequent iteration. The fix's
    /// global scan must observe that ANY persisted Tool row resolves
    /// the id, even if it sits inside a prior conversation round.
    #[test]
    fn should_not_synthesize_when_tool_result_exists_anywhere_in_transcript() {
        let mut msgs = vec![
            sys("prompt"),
            // Prior round: spawn_only run_pipeline kicked off. The
            // execution loop returned a handle Tool row with the
            // matching tool_call_id and the LLM moved on. The
            // cascade-fail timeout envelope (persisted as Assistant
            // by `persist_assistant_with_media`) followed asynchronously.
            assistant_with_tools(&["call_0_120"]),
            tool_result_msg("call_0_120"),
            // Background completion envelope: not a Tool row.
            Message {
                role: MessageRole::Assistant,
                content: "✗ run_pipeline failed: pipeline timed out after 1200s".to_string(),
                media: vec![],
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            user("继续追问"),
            // Current round: LLM re-emits the SAME spawn_only id
            // (deterministic providers can pick a stable id when the
            // prompt + tool shape repeat). Without the global scan,
            // the windowed look-ahead would walk past the
            // user/assistant boundary, find no Tool row, and
            // re-fabricate a `[result was lost]` placeholder every
            // turn.
            assistant_with_tools(&["call_0_120"]),
            user("(post-turn re-rendered prior history)"),
        ];
        let original_len = msgs.len();
        let changed = synthesize_missing_tool_results(&mut msgs);
        assert!(
            !changed,
            "global tool-result scan must observe the prior-round handle row and skip re-fab"
        );
        assert_eq!(msgs.len(), original_len);
    }

    /// NEW-11 regression: repeated invocations on the SAME transcript
    /// must converge — once a synthetic placeholder is inserted (or
    /// the row was already there), a subsequent pass must observe the
    /// resolution and refuse to insert a duplicate. Without this
    /// invariant the post-context-manager `prepare_conversation_messages`
    /// pass that fires on every iteration would emit a fresh
    /// `[result was lost]` Tool row on every loop tick — the persist
    /// loop that fleet-UX soak round-9 captured on mini3.
    #[test]
    fn should_be_idempotent_across_repeated_invocations() {
        let mut msgs = vec![
            sys("prompt"),
            assistant_with_tools(&["call_orphan"]),
            user("next question"),
        ];

        // First pass: synthesize the missing placeholder for `call_orphan`.
        let changed_first = synthesize_missing_tool_results(&mut msgs);
        assert!(changed_first);
        let after_first = msgs.len();

        // Second pass: the placeholder we inserted is now a Tool row in
        // the transcript, so the global scan must observe it and skip
        // re-fabrication.
        let changed_second = synthesize_missing_tool_results(&mut msgs);
        assert!(
            !changed_second,
            "second invocation must converge — no new placeholder for an already-resolved id"
        );
        assert_eq!(msgs.len(), after_first);

        // Third pass for good measure.
        let changed_third = synthesize_missing_tool_results(&mut msgs);
        assert!(!changed_third);
        assert_eq!(msgs.len(), after_first);
    }

    /// NEW-11 regression: when an assistant has tool_calls but only
    /// SOME of the ids are already resolved earlier in the transcript,
    /// only the truly-missing ids are synthesised. The resolved ids
    /// must be skipped even though they appear ABOVE the assistant
    /// (the spawn_only cascade-fail path can land its handle row in a
    /// position that `repair_message_order` collapsed earlier in the
    /// transcript).
    #[test]
    fn should_only_synthesize_truly_missing_when_some_resolved_above() {
        let mut msgs = vec![
            sys("prompt"),
            // Earlier in the transcript, `tc_resolved` has its Tool row.
            assistant_with_tools(&["tc_resolved"]),
            tool_result_msg("tc_resolved"),
            user("intermediate"),
            // Now a new assistant references BOTH the resolved id
            // (re-emitted by a deterministic provider) AND a fresh
            // truly-unresolved id.
            assistant_with_tools(&["tc_resolved", "tc_missing"]),
            user("trailing user prompt"),
        ];
        let changed = synthesize_missing_tool_results(&mut msgs);
        assert!(changed);
        // Find the synthesised placeholder for `tc_missing`.
        let synth_count = msgs
            .iter()
            .filter(|m| {
                m.role == MessageRole::Tool
                    && m.tool_call_id.as_deref() == Some("tc_missing")
                    && m.content.contains("lost")
            })
            .count();
        assert_eq!(synth_count, 1, "exactly one placeholder for the missing id");
        // No duplicate placeholder for the already-resolved id.
        let resolved_synth_count = msgs
            .iter()
            .filter(|m| {
                m.role == MessageRole::Tool
                    && m.tool_call_id.as_deref() == Some("tc_resolved")
                    && m.content.contains("lost")
            })
            .count();
        assert_eq!(
            resolved_synth_count, 0,
            "the resolved id keeps its original Tool row and must not gain a fabricated peer"
        );
    }
}
