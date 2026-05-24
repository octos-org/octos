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
/// 1. Walk assistant rows in document order. For each assistant with
///    tool_calls, gather matching Tool rows from the FULL transcript
///    (both before and after) — but track consumed Tool rows so a
///    later assistant that re-emits the same `tool_call_id`
///    (deterministic providers like DeepSeek can re-emit
///    `call_0_120`) cannot steal a result that already paired with an
///    earlier assistant.
/// 2. Re-insert exactly one result per tool_call right after each
///    assistant in the same order as `tool_calls`.
///
/// This handles backward-stranded results (e.g. from overflow tasks saving
/// results before the assistant message) AND the deterministic-id reuse
/// case (codex review on NEW-11): the pre-NEW-11 implementation walked
/// the transcript freshly per assistant and let the SECOND assistant
/// pull the tool row that had already been paired with the FIRST, then
/// `synthesize_missing_tool_results` inserted a placeholder for the
/// first — leaving the second assistant paired with a stale output. The
/// consumed-row tracking ensures one-tool-row-per-assistant pairing.
pub(crate) fn repair_message_order(messages: &mut Vec<Message>) -> bool {
    use std::collections::HashSet;

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

        // Collect expected tool_call IDs, preserving the call_ids order
        // for the re-insertion phase below.
        let call_ids: Vec<String> = messages[i]
            .tool_calls
            .as_ref()
            .map(|calls| calls.iter().map(|tc| tc.id.clone()).collect())
            .unwrap_or_default();
        let expected_ids: HashSet<String> = call_ids.iter().cloned().collect();

        // Helper: identify the assistant that "owns" the Tool row at
        // index `idx` — i.e. the nearest preceding non-Tool message
        // whose role is Assistant and whose `tool_calls` list contains
        // the tool's id. Returns `Some(idx)` when there is such an
        // owner, or `None` if the row is stranded.
        let owner_of = |idx: usize, messages: &[Message]| -> Option<usize> {
            let id = messages[idx].tool_call_id.clone()?;
            let mut k = idx;
            loop {
                if k == 0 {
                    return None;
                }
                k -= 1;
                if messages[k].role == MessageRole::Tool {
                    continue;
                }
                if messages[k].role == MessageRole::Assistant
                    && messages[k]
                        .tool_calls
                        .as_ref()
                        .is_some_and(|tcs| tcs.iter().any(|tc| tc.id == id))
                {
                    return Some(k);
                }
                return None;
            }
        };

        // Codex NEW-11 P2 (rev 3): gather Tool rows for the current
        // assistant's expected ids, removing each one to be re-inserted
        // in `tool_calls` order below. The ONLY guard is "refuse to
        // poach a row already paired with a DIFFERENT assistant"; any
        // row paired with the CURRENT assistant is still removed and
        // re-inserted so that duplicate or out-of-order rows in the
        // same block collapse to exactly one row per id, in
        // `tool_calls` order (codex review on rev 2 caught the
        // out-of-order case: `assistant([tc1, tc2]), tool(tc2),
        // tool(tc1)` must collapse to one tc1 + one tc2 in declared
        // order, not three rows). The `changed` flag may flip even
        // for a no-op rearrangement that happens to land in the same
        // position — callers treat `changed` as "a normalisation pass
        // ran", not "the wire shape differs". Tests cover both
        // directions: `should_be_stable_when_both_assistants_already_have_adjacent_tool_rows`
        // asserts the OUTPUT shape (not `changed`) so the contract
        // stays correct under the duplicate / out-of-order branch.
        let mut collected: std::collections::HashMap<String, Message> =
            std::collections::HashMap::new();
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
            if !is_match {
                j += 1;
                continue;
            }
            let id = messages[j].tool_call_id.clone().unwrap();
            let owner = owner_of(j, messages);
            if owner.is_some_and(|owner_idx| owner_idx != i) {
                // Already paired with a different assistant. Don't
                // poach — that assistant will (or already did) consume
                // it in its own iteration.
                j += 1;
                continue;
            }
            // Either truly stranded (owner == None) or owned by THIS
            // assistant (owner == Some(i)). Remove and re-insert at
            // the adjacent slot below — this also collapses
            // duplicate / out-of-order rows in the same block to one
            // result per id in declared order (codex P2 rev 3).
            let msg = messages.remove(j);
            changed = true;
            collected.insert(id, msg);
            if j < i {
                i -= 1;
            }
            // don't increment j -- removal shifted elements
        }

        // Re-insert one result per tool_call right after the
        // assistant, in the same order as tool_calls appear in the
        // assistant message.
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
/// NEW-11 invariant: providers validate `assistant(tool_calls) → tool` pairs
/// at the message boundary (the immediate contiguous block following each
/// assistant row), NOT by scanning the whole transcript. Two assistant rows
/// that reuse the SAME `tool_call_id` (e.g. a deterministic provider that
/// re-emits the same id when prompted with an identical conversation
/// shape, which fleet-UX soak round-9 observed on DeepSeek `call_0_120`)
/// must each get their own adjacent Tool row — a global "this id is
/// resolved somewhere" skip would break the second assistant's pairing
/// and re-introduce the 400 this helper is meant to prevent. The check
/// stays per-assistant + windowed; the NEW-11 hardening lives one level
/// up at the callsite (see `should_be_idempotent_within_a_single_pass`
/// below).
pub(crate) fn synthesize_missing_tool_results(messages: &mut Vec<Message>) -> bool {
    use std::collections::HashSet;

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

        // Synthesize placeholders for missing results. The check is per
        // assistant and per id within its OWN window: a re-used id under a
        // later assistant still needs its own adjacent placeholder when
        // no Tool row follows the LATER assistant (the windowed `existing`
        // set is rebuilt for each `i`).
        let insert_pos = j; // insert after existing tool results
        let mut inserted = 0;
        // NEW-11: track ids we've already synthesised UNDER THIS assistant
        // so the same id appearing twice in one assistant's tool_calls
        // list does not produce two adjacent placeholders. Providers
        // require one result per call, but identical id reuse within the
        // same assistant batch would otherwise insert N copies for N
        // duplicate ids.
        let mut synthesised_under_this_assistant: HashSet<String> = HashSet::new();
        for (id, name) in &call_ids {
            if existing.contains(id) || synthesised_under_this_assistant.contains(id) {
                continue;
            }
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
            synthesised_under_this_assistant.insert(id.clone());
            changed = true;
            inserted += 1;
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
    /// kicks off in iteration N, the execution-loop intercept returns
    /// a handle Tool row adjacent to its assistant. The windowed scan
    /// must observe that adjacent Tool row and skip re-fabrication —
    /// this is the steady-state pairing that fleet-UX soak round-9
    /// observed had been broken by a stale prior implementation
    /// (re-fab fired on every iteration). Codex review reaffirmed the
    /// per-assistant-window contract: providers validate
    /// `assistant(tool_calls) → tool` at the message boundary, so a
    /// global "this id resolved somewhere" skip would break a later
    /// assistant that re-emits the same id without an adjacent Tool
    /// row. The fix instead lives in the call-shape: the spawn_only
    /// intercept ALWAYS emits a handle Tool row adjacent to its
    /// assistant, and cascade-fail's failure envelope persists via
    /// `persist_assistant_with_media` so it does not collide with
    /// pairing. This test pins the steady-state shape so future
    /// regressions of either contract surface here.
    #[test]
    fn should_not_synthesize_when_spawn_only_handle_row_is_adjacent() {
        let mut msgs = vec![
            sys("prompt"),
            // Spawn_only intercept returns a handle Tool row adjacent
            // to the assistant. This is the canonical
            // `agent/execution.rs::spawn_only_handle_message` shape.
            assistant_with_tools(&["call_0_120"]),
            tool_result_msg("call_0_120"),
            // Background cascade-fail completion envelope is persisted
            // as Assistant (see `persist_assistant_with_media`), NOT
            // Tool — so it never participates in the assistant->tool
            // pairing check.
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
        ];
        let original_len = msgs.len();
        let changed = synthesize_missing_tool_results(&mut msgs);
        assert!(
            !changed,
            "windowed scan must observe the adjacent spawn_only handle Tool row and skip re-fab"
        );
        assert_eq!(msgs.len(), original_len);
    }

    /// NEW-11 regression: deterministic providers (DeepSeek observed
    /// `call_0_120` reuse on fleet-UX soak round-9) can re-emit the
    /// SAME `tool_call_id` from a later assistant when the
    /// conversation shape repeats. That second assistant MUST get
    /// its own adjacent Tool row even though an earlier assistant
    /// already has a Tool row for the same id — providers validate
    /// pairing at the message boundary, not globally.
    #[test]
    fn should_synthesize_per_assistant_when_same_id_reused_in_later_assistant() {
        let mut msgs = vec![
            sys("prompt"),
            // Earlier assistant has its adjacent Tool row.
            assistant_with_tools(&["call_0_120"]),
            tool_result_msg("call_0_120"),
            user("继续追问"),
            // Later assistant re-emits the same id but has NO adjacent
            // Tool row in the transcript. The windowed scan must
            // synthesise an adjacent placeholder for this assistant
            // even though the same id is already resolved above —
            // otherwise the provider sees an assistant with an
            // unresolved tool_call and 400s.
            assistant_with_tools(&["call_0_120"]),
            user("(post-turn re-rendered prior history)"),
        ];
        let changed = synthesize_missing_tool_results(&mut msgs);
        assert!(
            changed,
            "later assistant re-using the id must get its own adjacent placeholder"
        );
        // Locate the second assistant and confirm a placeholder Tool row
        // sits immediately after it.
        let second_assistant_idx = msgs
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                m.role == MessageRole::Assistant
                    && m.tool_calls
                        .as_ref()
                        .is_some_and(|tc| tc.iter().any(|c| c.id == "call_0_120"))
            })
            .map(|(idx, _)| idx)
            .nth(1)
            .expect("second assistant re-using the id is present");
        let adjacent = &msgs[second_assistant_idx + 1];
        assert_eq!(adjacent.role, MessageRole::Tool);
        assert_eq!(adjacent.tool_call_id.as_deref(), Some("call_0_120"));
        assert!(
            adjacent.content.contains("lost"),
            "the placeholder is the synthesised `[result was lost]` row"
        );
    }

    /// NEW-11 regression: a single pass MUST NOT insert two
    /// placeholders for the SAME id when one assistant's tool_calls
    /// list contains the same id twice (a degenerate but
    /// observed-in-the-wild provider shape on deeply-deterministic
    /// model paths). Providers reject this anyway, so we keep
    /// pairing 1:1 within the same assistant batch.
    #[test]
    fn should_not_double_synthesize_for_duplicate_id_within_same_assistant_batch() {
        let mut msgs = vec![
            sys("prompt"),
            // Assistant tool_calls list with a duplicate id.
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![
                    octos_core::ToolCall {
                        id: "dup_id".to_string(),
                        name: "tool_a".to_string(),
                        arguments: serde_json::json!({}),
                        metadata: None,
                    },
                    octos_core::ToolCall {
                        id: "dup_id".to_string(),
                        name: "tool_b".to_string(),
                        arguments: serde_json::json!({}),
                        metadata: None,
                    },
                ]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            user("next"),
        ];
        let changed = synthesize_missing_tool_results(&mut msgs);
        assert!(changed);
        let synth_count = msgs
            .iter()
            .filter(|m| m.role == MessageRole::Tool && m.tool_call_id.as_deref() == Some("dup_id"))
            .count();
        assert_eq!(
            synth_count, 1,
            "the same id within one assistant batch must produce exactly one placeholder"
        );
    }

    /// NEW-11 codex P2 regression: end-to-end coverage of the full
    /// repair pipeline (`repair_message_order` →
    /// `synthesize_missing_tool_results`) for the deterministic-id
    /// reuse case. Codex flagged that `repair_message_order`'s
    /// freshly-walk-the-transcript-per-assistant logic would let a
    /// LATER assistant steal a Tool row already paired with an
    /// EARLIER assistant, leaving the earlier assistant with a
    /// fabricated placeholder and the later assistant paired with
    /// stale output. The P2 fix in `repair_message_order` adds an
    /// "already paired with an earlier assistant" guard so the
    /// rightful first owner keeps its Tool row.
    #[test]
    fn should_keep_tool_paired_with_earlier_assistant_when_id_reused_by_later_assistant() {
        let mut msgs = vec![
            sys("prompt"),
            // First assistant emits id X. Its tool result follows
            // adjacently (the steady-state shape produced by the
            // execution loop).
            assistant_with_tools(&["call_0_120"]),
            tool_result_msg("call_0_120"),
            user("继续追问"),
            // Later assistant deterministically re-emits the same id.
            // No Tool row of its own exists in the transcript.
            assistant_with_tools(&["call_0_120"]),
            user("(post-turn re-rendered prior history)"),
        ];
        // Run the full repair pipeline in the order
        // `prepare_conversation_messages` would.
        repair_message_order(&mut msgs);
        repair_tool_pairs(&mut msgs);
        synthesize_missing_tool_results(&mut msgs);

        // The first assistant must STILL be paired with the original
        // (non-`lost`) Tool row.
        let first_assistant_idx = msgs
            .iter()
            .position(|m| {
                m.role == MessageRole::Assistant
                    && m.tool_calls
                        .as_ref()
                        .is_some_and(|tcs| tcs.iter().any(|tc| tc.id == "call_0_120"))
            })
            .expect("first assistant present");
        let first_pair = &msgs[first_assistant_idx + 1];
        assert_eq!(
            first_pair.role,
            MessageRole::Tool,
            "first assistant must keep an adjacent Tool row"
        );
        assert_eq!(first_pair.tool_call_id.as_deref(), Some("call_0_120"));
        assert!(
            !first_pair.content.contains("lost"),
            "first assistant must retain its ORIGINAL tool result, not a fabricated placeholder \
             (codex P2: stale-pairing prevention)"
        );

        // The second assistant must get its own adjacent placeholder
        // (synthesised) — providers validate per assistant.
        let second_assistant_idx = msgs
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                m.role == MessageRole::Assistant
                    && m.tool_calls
                        .as_ref()
                        .is_some_and(|tcs| tcs.iter().any(|tc| tc.id == "call_0_120"))
            })
            .map(|(idx, _)| idx)
            .nth(1)
            .expect("second assistant re-using the id is present");
        let second_pair = &msgs[second_assistant_idx + 1];
        assert_eq!(
            second_pair.role,
            MessageRole::Tool,
            "second assistant must also have an adjacent Tool row (synthesised)"
        );
        assert_eq!(second_pair.tool_call_id.as_deref(), Some("call_0_120"));
        assert!(
            second_pair.content.contains("lost"),
            "second assistant's adjacent Tool row is the synthesised `[result was lost]` placeholder"
        );
    }

    /// NEW-11 codex P2 rev-2 regression: when BOTH the earlier and
    /// later assistant referencing the same id ALREADY have
    /// adjacent Tool rows (the steady-state transcript after one
    /// repair pass on the deterministic-id reuse case), the OUTPUT
    /// pairing must survive intact across passes — the first
    /// assistant keeps its `real handle output`, the second keeps
    /// its `[result was lost]` placeholder, neither row is poached
    /// by the other side. (`changed` may still flip because the
    /// remove + same-slot re-insert flow is treated as
    /// "normalisation ran"; the wire shape is what matters and the
    /// contract is on the OUTPUT structure, not the bool.)
    #[test]
    fn should_be_stable_when_both_assistants_already_have_adjacent_tool_rows() {
        let mut msgs = vec![
            sys("prompt"),
            assistant_with_tools(&["call_0_120"]),
            Message {
                role: MessageRole::Tool,
                content: "real handle output".to_string(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_0_120".to_string()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            user("继续追问"),
            assistant_with_tools(&["call_0_120"]),
            Message {
                role: MessageRole::Tool,
                content: "[Tool 'run_pipeline' result was lost — no output available]".to_string(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_0_120".to_string()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            user("trailing"),
        ];
        let snapshot_before = msgs.clone();
        // Run twice to confirm convergence.
        let _ = repair_message_order(&mut msgs);
        let _ = repair_message_order(&mut msgs);
        assert_eq!(
            msgs.len(),
            snapshot_before.len(),
            "no rows added/removed across repeated passes"
        );
        // First assistant must retain its ORIGINAL tool row content.
        assert_eq!(msgs[2].role, MessageRole::Tool);
        assert_eq!(
            msgs[2].content, "real handle output",
            "first assistant keeps the real handle output, not the LATER lost placeholder"
        );
        // Second assistant must retain its lost-placeholder content.
        assert_eq!(msgs[5].role, MessageRole::Tool);
        assert!(
            msgs[5].content.contains("lost"),
            "second assistant keeps the lost placeholder, not the EARLIER real handle output"
        );
    }

    /// NEW-11 codex P2 rev-3 regression: a Tool block that is
    /// already adjacent to its assistant but contains duplicate or
    /// out-of-order rows must collapse to exactly one row per id,
    /// in `tool_calls` declared order. The intermediate "skip if
    /// already adjacent" optimisation in rev 2 would have left the
    /// duplicates in place AND inserted a fresh row alongside them.
    #[test]
    fn should_collapse_duplicate_or_out_of_order_tool_rows_to_one_per_id_in_declared_order() {
        let mut msgs = vec![
            sys("prompt"),
            // Assistant declares tool_calls in order [tc1, tc2], but
            // the existing Tool rows are reversed AND there is a
            // duplicate of tc1.
            assistant_with_tools(&["tc1", "tc2"]),
            tool_result_msg("tc2"),
            tool_result_msg("tc1"),
            tool_result_msg("tc1"),
            user("next"),
        ];
        repair_message_order(&mut msgs);
        // After repair: assistant at index 1 followed by exactly one
        // tc1 row (the LATEST per the HashMap insert semantic), then
        // one tc2 row, then user. Total 5 rows.
        assert_eq!(msgs.len(), 5);
        assert_eq!(msgs[0].role, MessageRole::System);
        assert_eq!(msgs[1].role, MessageRole::Assistant);
        assert_eq!(msgs[2].role, MessageRole::Tool);
        assert_eq!(msgs[2].tool_call_id.as_deref(), Some("tc1"));
        assert_eq!(msgs[3].role, MessageRole::Tool);
        assert_eq!(msgs[3].tool_call_id.as_deref(), Some("tc2"));
        assert_eq!(msgs[4].role, MessageRole::User);
    }

    /// NEW-11 regression: a single pass is internally idempotent
    /// over an already-paired transcript — repeated invocations on
    /// the SAME paired input do not insert extra placeholders.
    #[test]
    fn should_be_idempotent_within_a_single_pass_on_paired_input() {
        let mut msgs = vec![
            sys("prompt"),
            assistant_with_tools(&["tc1"]),
            tool_result_msg("tc1"),
            user("next"),
        ];
        let original_len = msgs.len();
        let changed_first = synthesize_missing_tool_results(&mut msgs);
        assert!(!changed_first, "no-op on already-paired input");
        let changed_second = synthesize_missing_tool_results(&mut msgs);
        assert!(!changed_second, "still no-op on the second pass");
        assert_eq!(msgs.len(), original_len);
    }
}
