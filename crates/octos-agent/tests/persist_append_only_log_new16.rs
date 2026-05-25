//! NEW-16 (fix/persist-from-append-only-turn-log-not-mutated-buffer):
//! Integration test that locks in codex's append-only per-turn output log
//! semantics for `ConversationResponse.messages`.
//!
//! Background:
//! `ConversationResponse.messages` used to be built by slicing the LLM
//! prompt buffer at `1 + history_len` (the system prompt + history
//! boundary). The buffer is mutated during the loop by
//! `prepare_conversation_messages` (which calls `repair_message_order`)
//! and — on the API server — by the AppUI bridge in
//! `ui_protocol.rs`. After mutation, OLD rows from prior turns could
//! end up past the stale boundary and get returned as "new". The WS
//! persist site reads `response.messages` blindly and writes each row
//! to JSONL, causing the cross-turn drag-forward duplicate-persistence
//! bug (mini3 Yuan-dynasty content showed up 7x in one session,
//! 2026-05-23 soak captures).
//!
//! Codex's fix (this PR): build an append-only per-turn output log
//! alongside the prompt buffer. The log is never read back from, only
//! pushed to — so no mutation pass can shift OLD rows into it.
//! `ConversationResponse.messages` is now `turn_output_log.clone()`,
//! independent of whatever rearrangement happened in the prompt
//! buffer.
//!
//! This test exercises the full agent loop with:
//!   1. Prior session history that contains a malformed/stranded Tool
//!      row (`tool_call_id` references a tool_call from a prior
//!      assistant message that did NOT yet have its result paired).
//!      This forces `repair_message_order` to do real work.
//!   2. A mock LLM provider that produces ToolUse -> EndTurn for the
//!      current turn (the canonical "tool call then summary" shape).
//!   3. An assertion that `response.messages` contains ONLY the rows
//!      this turn produced — current User + assistant(tool_calls) +
//!      Tool(result) + zero copies of the prior-turn rows that
//!      `repair_message_order` shuffled in the prompt buffer.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use octos_agent::{Agent, AgentConfig, ToolRegistry};
use octos_core::{AgentId, Message, MessageRole, ToolCall};
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage, ToolSpec};
use octos_memory::EpisodeStore;
use tempfile::TempDir;

/// Mirrors the `MockLlmProvider` in `integration.rs` but exposes the
/// last `messages` slice the loop sent us. We use this to confirm the
/// prompt buffer DID get mutated (i.e. `repair_message_order` did
/// real work) so the test is exercising the right path.
struct CapturingMockProvider {
    responses: Mutex<Vec<ChatResponse>>,
    last_messages: Mutex<Vec<Vec<Message>>>,
}

impl CapturingMockProvider {
    fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
            last_messages: Mutex::new(Vec::new()),
        }
    }

    fn captured_calls(&self) -> Vec<Vec<Message>> {
        self.last_messages.lock().unwrap().clone()
    }
}

#[async_trait]
impl LlmProvider for CapturingMockProvider {
    async fn chat(
        &self,
        messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        self.last_messages.lock().unwrap().push(messages.to_vec());
        let mut responses = self.responses.lock().unwrap();
        if responses.is_empty() {
            eyre::bail!("CapturingMockProvider: no more scripted responses");
        }
        Ok(responses.remove(0))
    }

    fn context_window(&self) -> u32 {
        128_000
    }

    fn model_id(&self) -> &str {
        "mock-model-new16"
    }

    fn provider_name(&self) -> &str {
        "mock-new16"
    }
}

fn tool_use_call(name: &str, id: &str, args: serde_json::Value) -> ChatResponse {
    ChatResponse {
        content: None,
        reasoning_content: None,
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: args,
            metadata: None,
        }],
        stop_reason: StopReason::ToolUse,
        usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        },
        provider_index: None,
    }
}

fn end_turn(text: &str) -> ChatResponse {
    ChatResponse {
        content: Some(text.to_string()),
        reasoning_content: None,
        tool_calls: vec![],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 8,
            output_tokens: 4,
            ..Default::default()
        },
        provider_index: None,
    }
}

/// Build a prior history that contains:
///   - A normal user/assistant exchange (chronological "Yuan-dynasty"
///     content — the soak-captured drag-forward bug)
///   - An assistant message with tool_calls
///   - A *stranded* Tool row (tool_call_id matches the assistant's
///     tool_call, but it lives FOUR rows after a User message, which
///     `repair_message_order` will gather and re-position).
///   - A second user-asks-something row (the "previous turn" that
///     completed before the current turn).
fn prior_history_with_stranded_tool_row() -> Vec<Message> {
    vec![
        Message {
            role: MessageRole::User,
            content: "Tell me about the Yuan dynasty.".to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: Some("prior-turn-a".into()),
            timestamp: chrono::Utc::now(),
        },
        Message {
            role: MessageRole::Assistant,
            content: String::new(),
            media: vec![],
            tool_calls: Some(vec![ToolCall {
                id: "old_call_yuan_1".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path": "yuan.txt"}),
                metadata: None,
            }]),
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: Some("prior-turn-a".into()),
            timestamp: chrono::Utc::now(),
        },
        // A user row separating the assistant's tool_call from its result
        // — exactly the shape that triggers `repair_message_order` to
        // gather the stranded Tool row and place it adjacent to the
        // assistant.
        Message {
            role: MessageRole::User,
            content: "(internal: continuing previous turn)".to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: Some("prior-turn-a".into()),
            timestamp: chrono::Utc::now(),
        },
        Message {
            role: MessageRole::Tool,
            content: "The Yuan dynasty (1271-1368) was founded by Kublai Khan.".to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: Some("old_call_yuan_1".into()),
            reasoning_content: None,
            client_message_id: None,
            thread_id: Some("prior-turn-a".into()),
            timestamp: chrono::Utc::now(),
        },
        Message {
            role: MessageRole::Assistant,
            content: "The Yuan dynasty was a Mongol-led dynasty in China from 1271 to 1368."
                .to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: Some("prior-turn-a".into()),
            timestamp: chrono::Utc::now(),
        },
    ]
}

/// Lock in NEW-16: with a stranded Tool row in prior history forcing
/// `repair_message_order` to re-position prompt-buffer rows, the
/// returned `ConversationResponse.messages` MUST contain ONLY rows
/// this turn produced (current user, current assistant w/ tool_calls,
/// current tool result), and ZERO prior-turn content.
#[tokio::test]
async fn process_message_returns_only_current_turn_rows_after_repair_runs() {
    let dir = TempDir::new().unwrap();

    // Stage a file the current turn's read_file will actually read so
    // the dispatcher returns success rather than a synthetic error.
    std::fs::write(dir.path().join("current.txt"), "current-turn-content").unwrap();

    let provider = Arc::new(CapturingMockProvider::new(vec![
        // iter 1: model calls read_file on a NEW file (current turn)
        tool_use_call(
            "read_file",
            "current_call_1",
            serde_json::json!({"path": "current.txt"}),
        ),
        // iter 2: model wraps up
        end_turn("Read current-turn-content."),
    ]));
    let llm: Arc<dyn LlmProvider> = provider.clone();
    let tools = ToolRegistry::with_builtins(dir.path());
    let memory = Arc::new(EpisodeStore::open(dir.path().join(".octos")).await.unwrap());
    let agent =
        Agent::new(AgentId::new("test-new16"), llm, tools, memory).with_config(AgentConfig {
            save_episodes: false,
            ..Default::default()
        });

    let history = prior_history_with_stranded_tool_row();
    let history_len = history.len();

    let resp = agent
        .process_message("Read current.txt", &history, vec![])
        .await
        .expect("agent should respond");

    // Sanity: the LLM saw a prompt where prior rows were rearranged
    // (repair_message_order moved the stranded Tool row adjacent to
    // its parent assistant). The post-prep prompt should NOT have
    // the same length as System + history + current_user + new rows
    // by simple addition — repair shuffled OLD rows in place. We
    // check this by confirming the prompt buffer contains the
    // stranded Tool row's `tool_call_id` ("old_call_yuan_1") AND
    // that the LLM saw more messages than just the new ones.
    // Sanity: confirm `repair_message_order` (or `normalize_tool_call_ids`)
    // did real work on the prompt buffer — i.e. the prompt the LLM saw
    // differs in structure from the raw `System + history + current_user`
    // concatenation. We check this by confirming the stranded Tool row
    // moved adjacent to its parent assistant (it was at index 4 in the
    // original history, behind a User row at index 3; after repair, the
    // Tool row must immediately follow its Assistant parent).
    let captured = provider.captured_calls();
    assert!(!captured.is_empty(), "provider received at least one call");
    let first_call_messages = &captured[0];
    // Locate the assistant-with-tool_calls and confirm the very next
    // row is the matching Tool, not the "(internal: continuing previous
    // turn)" User row that was originally between them.
    let asst_with_tcs_idx = first_call_messages
        .iter()
        .position(|m| {
            m.role == MessageRole::Assistant
                && m.tool_calls.as_ref().is_some_and(|tcs| !tcs.is_empty())
        })
        .expect("the prior assistant-with-tool_calls row should still appear in the prompt");
    let row_after = &first_call_messages
        .get(asst_with_tcs_idx + 1)
        .expect("there is a row after the assistant-with-tool_calls");
    assert_eq!(
        row_after.role,
        MessageRole::Tool,
        "repair_message_order should have moved the stranded Tool row to \
         immediately follow its parent Assistant. Got role {:?} instead. \
         This precondition guarantees the test is exercising the actual \
         prompt-buffer mutation path NEW-16 was reported against.",
        row_after.role
    );

    // The CORE assertion: `response.messages` is the per-turn append-only
    // output log — NOT a slice of the prompt buffer. It should contain
    // ONLY rows produced this turn:
    //   1. current user ("Read current.txt")
    //   2. assistant w/ tool_calls (call_id = "current_call_1")
    //   3. tool result for "current_call_1"
    //
    // It MUST NOT contain ANY prior-turn content (the Yuan-dynasty
    // assistant message, the stranded Tool row, etc.) even though
    // `repair_message_order` moved that content around in the prompt
    // buffer.
    assert!(
        !resp.messages.is_empty(),
        "response.messages should contain the current-turn rows"
    );

    // Inspect every row and assert it is current-turn material.
    for (i, msg) in resp.messages.iter().enumerate() {
        // No System rows ever (the loop drops the System prompt from
        // the slice equivalent).
        assert_ne!(
            msg.role,
            MessageRole::System,
            "response.messages[{i}] must not be a System row"
        );
        // No prior-turn Yuan content.
        assert!(
            !msg.content.contains("Yuan dynasty"),
            "response.messages[{i}] should NOT contain prior-turn 'Yuan dynasty' \
             content — that is the NEW-16 drag-forward bug. Got: {:?}",
            msg.content
        );
        assert!(
            !msg.content.contains("Mongol-led dynasty"),
            "response.messages[{i}] should NOT contain prior-turn Yuan summary"
        );
        assert!(
            !msg.content.contains("Kublai Khan"),
            "response.messages[{i}] should NOT contain prior-turn tool output \
             about Kublai Khan"
        );
        // No prior-turn tool_call_id.
        assert_ne!(
            msg.tool_call_id.as_deref(),
            Some("old_call_yuan_1"),
            "response.messages[{i}] should NOT carry the prior-turn stranded \
             tool_call_id 'old_call_yuan_1'"
        );
    }

    // Shape check: expect at least User + Assistant + Tool from this turn.
    let user_count = resp
        .messages
        .iter()
        .filter(|m| m.role == MessageRole::User)
        .count();
    let assistant_count = resp
        .messages
        .iter()
        .filter(|m| m.role == MessageRole::Assistant)
        .count();
    let tool_count = resp
        .messages
        .iter()
        .filter(|m| m.role == MessageRole::Tool)
        .count();
    assert_eq!(
        user_count, 1,
        "exactly one User row this turn — the current prompt; got {user_count} \
         in {:?}",
        resp.messages
    );
    assert!(
        assistant_count >= 1,
        "at least one Assistant row this turn (the ToolUse iteration); \
         got {assistant_count} in {:?}",
        resp.messages
    );
    assert_eq!(
        tool_count, 1,
        "exactly one Tool row this turn — the result of read_file on current.txt; \
         got {tool_count} in {:?}",
        resp.messages
    );

    // The User row is the CURRENT prompt, not any prior user.
    let current_user_row = resp
        .messages
        .iter()
        .find(|m| m.role == MessageRole::User)
        .unwrap();
    assert_eq!(current_user_row.content, "Read current.txt");

    // The Tool row references the CURRENT turn's tool_call_id.
    let current_tool_row = resp
        .messages
        .iter()
        .find(|m| m.role == MessageRole::Tool)
        .unwrap();
    assert_eq!(
        current_tool_row.tool_call_id.as_deref(),
        Some("current_call_1"),
        "the Tool row in response.messages should reference the current turn's \
         tool_call_id, not any prior turn's"
    );

    // Sanity: `history_len` did NOT determine response.messages length.
    // Under the OLD code path, `repair_message_order` could shift
    // prior rows past `1 + history_len` and bloat the slice. Under
    // NEW-16 the count is independent of history_len.
    assert!(
        resp.messages.len() < history_len,
        "response.messages.len() should be far smaller than history.len() ({history_len}) — \
         it should only contain the rows produced THIS turn. Got {} rows.",
        resp.messages.len()
    );

    // Final answer is the EndTurn content.
    assert_eq!(resp.content, "Read current-turn-content.");
}
