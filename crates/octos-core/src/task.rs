//! Task model: atomic unit of work.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::{AgentId, EpisodeRef, Message, TaskId};

/// A task is an atomic unit of work assigned to an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    /// Unique identifier.
    pub id: TaskId,
    /// Parent task ID (for subtasks).
    pub parent_id: Option<TaskId>,
    /// Current status.
    pub status: TaskStatus,
    /// What kind of task this is.
    pub kind: TaskKind,
    /// Context passed to the agent.
    pub context: TaskContext,
    /// Result after completion (if any).
    pub result: Option<TaskResult>,
    /// When the task was created.
    pub created_at: DateTime<Utc>,
    /// When the task was last updated.
    pub updated_at: DateTime<Utc>,
}

impl Task {
    /// Create a new task with the given kind and context.
    pub fn new(kind: TaskKind, context: TaskContext) -> Self {
        let now = Utc::now();
        Self {
            id: TaskId::new(),
            parent_id: None,
            status: TaskStatus::Pending,
            kind,
            context,
            result: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// Create a subtask of this task.
    pub fn subtask(&self, kind: TaskKind) -> Self {
        let now = Utc::now();
        Self {
            id: TaskId::new(),
            parent_id: Some(self.id.clone()),
            status: TaskStatus::Pending,
            kind,
            context: self.context.clone(),
            result: None,
            created_at: now,
            updated_at: now,
        }
    }
}

/// Task execution status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TaskStatus {
    /// Waiting to be assigned.
    Pending,
    /// Currently being executed by an agent.
    InProgress { agent_id: AgentId },
    /// Blocked waiting for something.
    Blocked { reason: String },
    /// Successfully completed.
    Completed,
    /// Failed with an error.
    Failed { error: String },
}

/// What kind of work the task represents.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TaskKind {
    /// Plan: decompose a goal into subtasks.
    Plan { goal: String },
    /// Code: write or modify code.
    Code {
        instruction: String,
        files: Vec<PathBuf>,
    },
    /// Review: review code changes.
    Review { diff: String },
    /// Test: run tests or verification.
    Test { command: String },
    /// Custom task type.
    Custom {
        name: String,
        params: serde_json::Value,
    },
}

/// Context passed to an agent when executing a task.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskContext {
    /// Working directory for the task.
    pub working_dir: PathBuf,
    /// Git state (branch, uncommitted changes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_state: Option<GitState>,
    /// Recent conversation turns (working memory).
    pub working_memory: Vec<Message>,
    /// References to relevant past episodes.
    pub episodic_refs: Vec<EpisodeRef>,
    /// Files in scope for this task.
    pub files_in_scope: Vec<PathBuf>,
}

/// Git repository state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitState {
    pub branch: String,
    pub has_uncommitted_changes: bool,
    pub head_commit: Option<String>,
}

/// Result of task execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResult {
    /// Whether the task succeeded.
    pub success: bool,
    /// Output or summary.
    pub output: String,
    /// Files that were modified.
    pub files_modified: Vec<PathBuf>,
    /// Subtasks created (for Plan tasks).
    pub subtasks: Vec<TaskId>,
    /// Token usage.
    pub token_usage: TokenUsage,
}

/// Token usage statistics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// Tokens used for internal chain-of-thought (reasoning models).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub reasoning_tokens: u32,
    /// Tokens served from provider cache.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cache_read_tokens: u32,
    /// Tokens written to provider cache.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cache_write_tokens: u32,
}

fn is_zero(v: &u32) -> bool {
    *v == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_new() {
        let task = Task::new(
            TaskKind::Plan {
                goal: "test goal".to_string(),
            },
            TaskContext::default(),
        );

        assert_eq!(task.status, TaskStatus::Pending);
        assert!(task.parent_id.is_none());
        assert!(task.result.is_none());
    }

    #[test]
    fn test_task_subtask() {
        let parent = Task::new(
            TaskKind::Plan {
                goal: "parent goal".to_string(),
            },
            TaskContext {
                working_dir: PathBuf::from("/test"),
                ..Default::default()
            },
        );

        let child = parent.subtask(TaskKind::Code {
            instruction: "implement feature".to_string(),
            files: vec![],
        });

        assert_eq!(child.parent_id, Some(parent.id.clone()));
        assert_eq!(child.status, TaskStatus::Pending);
        assert_eq!(child.context.working_dir, parent.context.working_dir);
    }

    #[test]
    fn test_task_status_serialization() {
        let status = TaskStatus::InProgress {
            agent_id: crate::AgentId::new("test-agent"),
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("in_progress"));
        assert!(json.contains("test-agent"));

        let parsed: TaskStatus = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, TaskStatus::InProgress { .. }));
    }

    #[test]
    fn test_task_kind_serialization() {
        let kind = TaskKind::Code {
            instruction: "fix bug".to_string(),
            files: vec![PathBuf::from("src/main.rs")],
        };
        let json = serde_json::to_string(&kind).unwrap();
        assert!(json.contains("code"));
        assert!(json.contains("fix bug"));

        let parsed: TaskKind = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, TaskKind::Code { .. }));
    }

    #[test]
    fn test_token_usage_default() {
        let usage = TokenUsage::default();
        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.output_tokens, 0);
    }

    #[test]
    fn test_task_kind_plan_serde() {
        let kind = TaskKind::Plan {
            goal: "deploy app".to_string(),
        };
        let json = serde_json::to_string(&kind).unwrap();
        assert!(json.contains("plan"));
        assert!(json.contains("deploy app"));
        let parsed: TaskKind = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, TaskKind::Plan { .. }));
    }

    #[test]
    fn test_task_kind_review_serde() {
        let kind = TaskKind::Review {
            diff: "+line1\n-line2".to_string(),
        };
        let json = serde_json::to_string(&kind).unwrap();
        assert!(json.contains("review"));
        let parsed: TaskKind = serde_json::from_str(&json).unwrap();
        match parsed {
            TaskKind::Review { diff } => assert!(diff.contains("+line1")),
            _ => panic!("expected Review"),
        }
    }

    #[test]
    fn test_task_kind_test_serde() {
        let kind = TaskKind::Test {
            command: "cargo test".to_string(),
        };
        let json = serde_json::to_string(&kind).unwrap();
        assert!(json.contains("\"test\""));
        let parsed: TaskKind = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, TaskKind::Test { .. }));
    }

    #[test]
    fn test_task_kind_custom_serde() {
        let kind = TaskKind::Custom {
            name: "deploy".to_string(),
            params: serde_json::json!({"env": "staging"}),
        };
        let json = serde_json::to_string(&kind).unwrap();
        assert!(json.contains("custom"));
        let parsed: TaskKind = serde_json::from_str(&json).unwrap();
        match parsed {
            TaskKind::Custom { name, params } => {
                assert_eq!(name, "deploy");
                assert_eq!(params["env"], "staging");
            }
            _ => panic!("expected Custom"),
        }
    }

    #[test]
    fn test_task_status_blocked_serde() {
        let status = TaskStatus::Blocked {
            reason: "waiting for review".to_string(),
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("blocked"));
        let parsed: TaskStatus = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, TaskStatus::Blocked { .. }));
    }

    #[test]
    fn test_task_status_failed_serde() {
        let status = TaskStatus::Failed {
            error: "timeout".to_string(),
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("failed"));
        let parsed: TaskStatus = serde_json::from_str(&json).unwrap();
        match parsed {
            TaskStatus::Failed { error } => assert_eq!(error, "timeout"),
            _ => panic!("expected Failed"),
        }
    }

    #[test]
    fn test_task_status_completed_serde() {
        let status = TaskStatus::Completed;
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("completed"));
        let parsed: TaskStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, TaskStatus::Completed);
    }

    #[test]
    fn test_task_result_serde() {
        let result = TaskResult {
            success: true,
            output: "all tests pass".to_string(),
            files_modified: vec![PathBuf::from("src/main.rs")],
            subtasks: vec![],
            token_usage: TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                ..Default::default()
            },
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: TaskResult = serde_json::from_str(&json).unwrap();
        assert!(parsed.success);
        assert_eq!(parsed.output, "all tests pass");
        assert_eq!(parsed.token_usage.input_tokens, 100);
    }

    #[test]
    fn test_task_context_default() {
        let ctx = TaskContext::default();
        assert_eq!(ctx.working_dir, PathBuf::new());
        assert!(ctx.git_state.is_none());
        assert!(ctx.working_memory.is_empty());
        assert!(ctx.episodic_refs.is_empty());
        assert!(ctx.files_in_scope.is_empty());
    }

    #[test]
    fn test_git_state_serde() {
        let state = GitState {
            branch: "main".to_string(),
            has_uncommitted_changes: true,
            head_commit: Some("abc123".to_string()),
        };
        let json = serde_json::to_string(&state).unwrap();
        let parsed: GitState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.branch, "main");
        assert!(parsed.has_uncommitted_changes);
        assert_eq!(parsed.head_commit.as_deref(), Some("abc123"));
    }
}
