//! Message protocol between agents.

use serde::{Deserialize, Serialize};

use crate::task::{Task, TaskResult, TaskStatus};
use crate::types::{Message, TaskId};

/// Messages exchanged between agents in the coordination protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentMessage {
    /// Coordinator assigns a task to a worker.
    TaskAssign { task: Box<Task> },

    /// Agent updates the status of a task.
    TaskUpdate { task_id: TaskId, status: TaskStatus },

    /// Agent completes a task with a result.
    TaskComplete { task_id: TaskId, result: TaskResult },

    /// Agent requests context for a task.
    ContextRequest { task_id: TaskId, query: String },

    /// Response to a context request.
    ContextResponse {
        task_id: TaskId,
        context: Vec<Message>,
    },
}

impl AgentMessage {
    /// Get the task ID this message relates to (if any).
    pub fn task_id(&self) -> Option<&TaskId> {
        match self {
            Self::TaskAssign { task } => Some(&task.id),
            Self::TaskUpdate { task_id, .. } => Some(task_id),
            Self::TaskComplete { task_id, .. } => Some(task_id),
            Self::ContextRequest { task_id, .. } => Some(task_id),
            Self::ContextResponse { task_id, .. } => Some(task_id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{Task, TaskContext, TaskKind, TaskResult, TokenUsage};
    use crate::types::AgentId;
    use std::path::PathBuf;

    fn make_task() -> Task {
        Task::new(
            TaskKind::Code {
                instruction: "fix bug".into(),
                files: vec![],
            },
            TaskContext::default(),
        )
    }

    fn make_result() -> TaskResult {
        TaskResult {
            success: true,
            output: "done".into(),
            files_modified: vec![],
            subtasks: vec![],
            token_usage: TokenUsage::default(),
        }
    }

    #[test]
    fn test_task_assign_serde_roundtrip() {
        let msg = AgentMessage::TaskAssign {
            task: Box::new(make_task()),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"task_assign\""));
        let back: AgentMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, AgentMessage::TaskAssign { .. }));
    }

    #[test]
    fn test_task_update_serde_roundtrip() {
        let msg = AgentMessage::TaskUpdate {
            task_id: TaskId::new(),
            status: TaskStatus::InProgress {
                agent_id: AgentId::new("agent-1"),
            },
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: AgentMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, AgentMessage::TaskUpdate { .. }));
    }

    #[test]
    fn test_task_complete_serde_roundtrip() {
        let msg = AgentMessage::TaskComplete {
            task_id: TaskId::new(),
            result: make_result(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: AgentMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, AgentMessage::TaskComplete { .. }));
    }

    #[test]
    fn test_context_request_serde_roundtrip() {
        let msg = AgentMessage::ContextRequest {
            task_id: TaskId::new(),
            query: "find related code".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("find related code"));
        let back: AgentMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, AgentMessage::ContextRequest { .. }));
    }

    #[test]
    fn test_context_response_serde_roundtrip() {
        let msg = AgentMessage::ContextResponse {
            task_id: TaskId::new(),
            context: vec![],
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: AgentMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, AgentMessage::ContextResponse { .. }));
    }

    #[test]
    fn test_task_id_returns_some_for_all_variants() {
        let id = TaskId::new();
        let msgs = vec![
            AgentMessage::TaskAssign {
                task: Box::new(make_task()),
            },
            AgentMessage::TaskUpdate {
                task_id: id.clone(),
                status: TaskStatus::Pending,
            },
            AgentMessage::TaskComplete {
                task_id: id.clone(),
                result: make_result(),
            },
            AgentMessage::ContextRequest {
                task_id: id.clone(),
                query: "q".into(),
            },
            AgentMessage::ContextResponse {
                task_id: id,
                context: vec![],
            },
        ];

        for msg in &msgs {
            assert!(msg.task_id().is_some());
        }
    }
}
