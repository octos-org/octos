//! Turn type semantics for distinguishing message categories in the agent loop.
//!
//! Provides a typed wrapper around `Message` that distinguishes between
//! user input, tool results, steering injections, and system context,
//! enabling the agent loop to handle each category differently.
//!
//! TODO: Wire into agent loop to replace raw `Message` handling with typed turns.

use octos_core::{Message, MessageRole};

/// The semantic type of a turn in the agent conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnKind {
    /// Direct user input (typed or spoken).
    UserInput,
    /// Result from a tool execution.
    ToolResult { tool_name: String },
    /// Injected follow-up from steering channel.
    SteeringFollowUp,
    /// System-level reminder injected between turns.
    SystemReminder,
    /// Agent's own response / reasoning.
    AssistantResponse,
    /// Context from memory or retrieved documents.
    RetrievedContext,
}

/// A typed turn wrapping a `Message` with semantic metadata.
#[derive(Debug, Clone)]
pub struct Turn {
    /// The underlying message.
    pub message: Message,
    /// Semantic classification of this turn.
    pub kind: TurnKind,
    /// Iteration number when this turn was added (0-based).
    pub iteration: u32,
}

impl Turn {
    /// Create a new turn.
    pub fn new(message: Message, kind: TurnKind, iteration: u32) -> Self {
        Self {
            message,
            kind,
            iteration,
        }
    }

    /// Create a user input turn.
    pub fn user_input(content: impl Into<String>) -> Self {
        Self {
            message: Message::user(content),
            kind: TurnKind::UserInput,
            iteration: 0,
        }
    }

    /// Create a tool result turn.
    pub fn tool_result(
        tool_name: impl Into<String>,
        content: impl Into<String>,
        tool_call_id: impl Into<String>,
    ) -> Self {
        Self {
            message: Message {
                role: MessageRole::Tool,
                content: content.into(),
                tool_call_id: Some(tool_call_id.into()),
                media: vec![],
                tool_calls: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            kind: TurnKind::ToolResult {
                tool_name: tool_name.into(),
            },
            iteration: 0,
        }
    }

    /// Create a steering follow-up turn.
    pub fn steering(content: impl Into<String>) -> Self {
        Self {
            message: Message::user(content),
            kind: TurnKind::SteeringFollowUp,
            iteration: 0,
        }
    }

    /// Create a system reminder turn.
    pub fn system_reminder(content: impl Into<String>) -> Self {
        Self {
            message: Message {
                role: MessageRole::System,
                content: content.into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            kind: TurnKind::SystemReminder,
            iteration: 0,
        }
    }

    /// Set the iteration number.
    pub fn at_iteration(mut self, iteration: u32) -> Self {
        self.iteration = iteration;
        self
    }

    /// Check if this is a user-originated turn (input or steering).
    pub fn is_user_originated(&self) -> bool {
        matches!(self.kind, TurnKind::UserInput | TurnKind::SteeringFollowUp)
    }

    /// Check if this is a tool result.
    pub fn is_tool_result(&self) -> bool {
        matches!(self.kind, TurnKind::ToolResult { .. })
    }

    /// Extract tool name if this is a tool result.
    pub fn tool_name(&self) -> Option<&str> {
        match &self.kind {
            TurnKind::ToolResult { tool_name } => Some(tool_name),
            _ => None,
        }
    }
}

/// Convert a sequence of turns back to plain messages for LLM calls.
pub fn turns_to_messages(turns: &[Turn]) -> Vec<Message> {
    turns.iter().map(|t| t.message.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_create_user_input() {
        let turn = Turn::user_input("Hello");
        assert_eq!(turn.kind, TurnKind::UserInput);
        assert!(turn.is_user_originated());
        assert!(!turn.is_tool_result());
        assert_eq!(turn.message.role, MessageRole::User);
    }

    #[test]
    fn should_create_tool_result() {
        let turn = Turn::tool_result("shell", "output", "call_1");
        assert!(turn.is_tool_result());
        assert_eq!(turn.tool_name(), Some("shell"));
        assert_eq!(turn.message.role, MessageRole::Tool);
        assert_eq!(turn.message.tool_call_id.as_deref(), Some("call_1"));
    }

    #[test]
    fn should_create_steering() {
        let turn = Turn::steering("focus on tests");
        assert_eq!(turn.kind, TurnKind::SteeringFollowUp);
        assert!(turn.is_user_originated());
    }

    #[test]
    fn should_create_system_reminder() {
        let turn = Turn::system_reminder("Remember the rules");
        assert_eq!(turn.kind, TurnKind::SystemReminder);
        assert!(!turn.is_user_originated());
        assert_eq!(turn.message.role, MessageRole::System);
    }

    #[test]
    fn should_set_iteration() {
        let turn = Turn::user_input("Hi").at_iteration(5);
        assert_eq!(turn.iteration, 5);
    }

    #[test]
    fn should_convert_turns_to_messages() {
        let turns = vec![
            Turn::user_input("Hello"),
            Turn::tool_result("shell", "ok", "c1"),
        ];
        let messages = turns_to_messages(&turns);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, MessageRole::User);
        assert_eq!(messages[1].role, MessageRole::Tool);
    }

    #[test]
    fn should_distinguish_kinds() {
        assert_ne!(TurnKind::UserInput, TurnKind::SteeringFollowUp);
        assert_ne!(
            TurnKind::ToolResult {
                tool_name: "a".into()
            },
            TurnKind::ToolResult {
                tool_name: "b".into()
            }
        );
    }
}
