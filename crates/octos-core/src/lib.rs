//! Core types, task model, and protocols for octos.
//!
//! This crate defines the foundational types used across all octos crates:
//! - Task model (Task, TaskStatus, TaskKind)
//! - Agent roles and identifiers
//! - Message protocol between agents
//! - Context and result types

pub mod abort;
pub mod app_ui;
pub mod app_ui_codec;
mod error;
pub mod gateway;
mod message;
pub mod session_scope;
mod task;
mod types;
pub mod ui_protocol;
mod utils;

pub use abort::{abort_response, is_abort_trigger};
pub use error::{Error, ErrorKind, Result};
pub use gateway::{InboundMessage, METADATA_SENDER_USER_ID, OutboundMessage};
pub use message::AgentMessage;
pub use session_scope::{
    DEFAULT_MULTI_TENANT_SHARED_ZONE_NAMES, MULTI_TENANT_USERS_DIR_NAME,
    MULTI_TENANT_WORKSPACE_DIR_NAME, PathClassification, SESSION_SCOPE_SCHEMA_VERSION, ScopeMode,
    SessionScope, SessionScopeError, is_safe_session_id,
};
pub use task::{
    DecisionRecord, FileRecord, SESSION_SUMMARY_SCHEMA_VERSION, STALE_DECISION_PREFIX,
    SessionSummary, TASK_RESULT_SCHEMA_VERSION, Task, TaskContext, TaskKind, TaskResult,
    TaskStatus, TokenUsage, UnsupportedSessionSummaryVersion,
};
pub use types::{
    AgentId, ClientMessageId, EpisodeRef, IdentityError, IdentityKind, MAIN_PROFILE_ID, Message,
    MessageRole, SessionKey, TaskId, ThreadId, ToolCall, TurnId,
};
pub use ui_protocol::{EventEnvelope, TurnContext};
pub use utils::{tool_output_limit, truncate_head_tail, truncate_utf8, truncated_utf8};
