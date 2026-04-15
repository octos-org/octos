//! Message preparation pipeline for long-turn loop calls.
//!
//! The loop runner previously inlined a fixed sequence of trimming, repair,
//! and normalization steps before each model call.  Keeping that sequence in a
//! dedicated helper makes the behavior easier to exercise in isolation without
//! booting the full agent loop.

use octos_core::Message;

use super::Agent;
use super::message_repair::{
    normalize_system_messages, normalize_tool_call_ids, repair_message_order, repair_tool_pairs,
    synthesize_missing_tool_results, truncate_old_tool_results,
};

/// Prepare a conversation turn for the next model call.
///
/// This keeps the existing behavior stable while centralizing the order of the
/// cleanup passes:
/// 1. trim to the context window
/// 2. normalize system messages
/// 3. repair tool ordering/pairs
/// 4. synthesize missing tool results as a last resort
/// 5. truncate old tool outputs
/// 6. normalize tool call IDs
pub(crate) fn prepare_conversation_messages(agent: &Agent, messages: &mut Vec<Message>) {
    agent.trim_to_context_window(messages);
    normalize_system_messages(messages);
    repair_message_order(messages);
    repair_tool_pairs(messages);
    synthesize_missing_tool_results(messages);
    truncate_old_tool_results(messages);
    normalize_tool_call_ids(messages);
}

/// Prepare a task turn for the next model call.
///
/// Task loops currently only need context trimming plus tool-call ID
/// normalization.  Keeping this as a dedicated helper makes the task loop
/// testable without the surrounding orchestration.
pub(crate) fn prepare_task_messages(agent: &Agent, messages: &mut Vec<Message>) {
    agent.trim_to_context_window(messages);
    normalize_tool_call_ids(messages);
}

#[cfg(test)]
mod tests {
    use super::*;

    use async_trait::async_trait;
    use eyre::Result;
    use octos_core::{AgentId, MessageRole, ToolCall};
    use octos_llm::{ChatConfig, ChatResponse, LlmProvider, ToolSpec};
    use octos_memory::EpisodeStore;
    use std::sync::Arc;
    use tempfile::TempDir;

    use crate::tools::ToolRegistry;

    struct SmallWindowProvider {
        window: u32,
    }

    #[async_trait]
    impl LlmProvider for SmallWindowProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> Result<ChatResponse> {
            eyre::bail!("not used in compaction tests");
        }

        fn context_window(&self) -> u32 {
            self.window
        }

        fn model_id(&self) -> &str {
            "mock-model"
        }

        fn provider_name(&self) -> &str {
            "mock"
        }
    }

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

    fn assistant(content: &str) -> Message {
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

    fn assistant_with_tools(tool_ids: &[&str]) -> Message {
        Message {
            role: MessageRole::Assistant,
            content: String::new(),
            media: vec![],
            tool_calls: Some(
                tool_ids
                    .iter()
                    .map(|id| ToolCall {
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

    fn tool_result_msg(id: &str, content: &str) -> Message {
        Message {
            role: MessageRole::Tool,
            content: content.to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: Some(id.to_string()),
            reasoning_content: None,
            timestamp: chrono::Utc::now(),
        }
    }

    async fn setup_agent(window: u32) -> (TempDir, Agent) {
        let dir = TempDir::new().unwrap();
        let provider: Arc<dyn LlmProvider> = Arc::new(SmallWindowProvider { window });
        let tools = ToolRegistry::with_builtins(dir.path());
        let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
        let agent = Agent::new(AgentId::new("test-agent"), provider, tools, memory);
        (dir, agent)
    }

    fn projection(messages: &[Message]) -> Vec<(MessageRole, String, Option<String>, Vec<String>)> {
        messages
            .iter()
            .map(|m| {
                let tool_ids = m
                    .tool_calls
                    .as_ref()
                    .map(|calls| calls.iter().map(|c| c.id.clone()).collect())
                    .unwrap_or_default();
                (
                    m.role.clone(),
                    m.content.clone(),
                    m.tool_call_id.clone(),
                    tool_ids,
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn should_budget_oversized_tool_outputs_deterministically() {
        let (_dir, agent) = setup_agent(300).await;
        let huge = "x".repeat(5_000);
        let base = vec![
            sys("prompt"),
            user("old filler 1"),
            assistant("old reply 1"),
            user("old filler 2"),
            assistant("old reply 2"),
            assistant_with_tools(&["call_1"]),
            tool_result_msg("call_1", &huge),
            user("current question"),
        ];

        let mut first = base.clone();
        let mut second = base.clone();
        prepare_conversation_messages(&agent, &mut first);
        prepare_conversation_messages(&agent, &mut second);

        assert_eq!(projection(&first), projection(&second));
        let truncated_tool = first
            .iter()
            .find(|m| m.role == MessageRole::Tool && m.tool_call_id.as_deref() == Some("call_1"))
            .expect("missing truncated old tool result");
        assert!(
            truncated_tool
                .content
                .contains("[... truncated for brevity]")
        );
        assert!(truncated_tool.content.len() <= 830);
    }

    #[tokio::test]
    async fn should_preserve_tool_result_validity_after_compaction() {
        let (_dir, agent) = setup_agent(300).await;
        let huge = "y".repeat(5_000);
        let mut messages = vec![
            sys("prompt"),
            user("old filler 1"),
            assistant("old reply 1"),
            user("old filler 2"),
            assistant("old reply 2"),
            assistant_with_tools(&["call_recent"]),
            tool_result_msg("call_recent", &huge),
            user("current question"),
        ];

        prepare_conversation_messages(&agent, &mut messages);

        let assistant_idx = messages
            .iter()
            .position(|m| {
                m.role == MessageRole::Assistant
                    && m.tool_calls
                        .as_ref()
                        .is_some_and(|calls| calls.iter().any(|c| c.id == "call_recent"))
            })
            .expect("missing recent assistant tool call");
        assert_eq!(
            messages[assistant_idx + 1].role,
            MessageRole::Tool,
            "recent tool result must remain adjacent to its assistant"
        );
        assert_eq!(
            messages[assistant_idx + 1].tool_call_id.as_deref(),
            Some("call_recent")
        );
        assert!(
            messages[assistant_idx + 1]
                .content
                .contains("[... truncated for brevity]"),
            "recent oversized tool output should be budgeted deterministically"
        );

        assert!(
            messages
                .iter()
                .all(|m| m.role != MessageRole::Tool || m.tool_call_id.is_some())
        );
    }
}
