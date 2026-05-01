//! Core types, task model, and protocols for octos.
//!
//! This crate defines the foundational types used across all octos crates:
//! - Task model (Task, TaskStatus, TaskKind)
//! - Agent roles and identifiers
//! - Message protocol between agents
//! - Context and result types

pub mod abort;
pub mod app_ui;
mod error;
pub mod gateway;
mod message;
mod task;
mod types;
pub mod ui_protocol;
mod utils;

pub use abort::{abort_response, is_abort_trigger};
pub use error::{Error, ErrorKind, Result};
pub use gateway::{InboundMessage, METADATA_SENDER_USER_ID, OutboundMessage};
pub use message::AgentMessage;
pub use task::{
    DecisionRecord, FileRecord, SESSION_SUMMARY_SCHEMA_VERSION, STALE_DECISION_PREFIX,
    SessionSummary, TASK_RESULT_SCHEMA_VERSION, Task, TaskContext, TaskKind, TaskResult,
    TaskStatus, TokenUsage, UnsupportedSessionSummaryVersion,
};
pub use types::{
    AgentId, ClientMessageId, EpisodeRef, IdentityError, IdentityKind, MAIN_PROFILE_ID, Message,
    MessageRole, SessionKey, TaskId, ThreadId, ToolCall, TurnId,
};
pub use utils::{tool_output_limit, truncate_head_tail, truncate_utf8, truncated_utf8};
