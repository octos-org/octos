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
