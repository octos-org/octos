//! Core types, task model, and protocols for octos.
//!
//! This crate defines the foundational types used across all octos crates:
//! - Task model (Task, TaskStatus, TaskKind)
//! - Agent roles and identifiers
//! - Message protocol between agents
//! - Context and result types

pub mod abort;
mod error;
pub mod gateway;
mod message;
mod task;
mod types;
mod utils;

pub use abort::is_abort_trigger;
pub use error::{Error, ErrorKind, Result};
pub use gateway::{InboundMessage, METADATA_SENDER_USER_ID, OutboundMessage};
pub use message::AgentMessage;
pub use task::{Task, TaskContext, TaskKind, TaskResult, TaskStatus, TokenUsage};
pub use types::{
    AgentId, EpisodeRef, MAIN_PROFILE_ID, Message, MessageRole, SessionKey, TaskId, ToolCall,
};
pub use utils::{tool_output_limit, truncate_head_tail, truncate_utf8, truncated_utf8};
