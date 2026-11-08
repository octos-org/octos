//! Interactive human-blocking bridges used by tools: the approval
//! bridge (`TOOL_APPROVAL_CTX`) and the structured user-question bridge
//! (`USER_QUESTION_CTX`, UPCR-2026-023). Extracted from `tools/mod.rs` to
//! keep the module under the 3000-line quality-ratchet ceiling; the
//! public names are re-exported from `tools::mod` unchanged.

use std::sync::Arc;

use async_trait::async_trait;

/// Request emitted by a tool when runtime policy requires user approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolApprovalRequest {
    pub tool_id: String,
    pub tool_name: String,
    pub title: String,
    pub body: String,
    pub command: Option<String>,
    pub cwd: Option<String>,
    /// This approval covers exactly this call: a remembered approval scope
    /// (`approve_for_tool`, `approve_for_session`, …) must neither answer it
    /// nor be recorded from it. Set by host-routed app tools (UPCR-2026-035),
    /// whose approvals carry the exact arguments.
    pub once_only: bool,
    /// A host-routed app tool's call (UPCR-2026-035): the owning app, the
    /// tool, the exact arguments and the caller, for the host's own sheet.
    pub host_tool: Option<octos_core::ui_protocol::ApprovalHostToolDetails>,
}

/// Decision returned to a blocked tool after client approval handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolApprovalDecision {
    Approve,
    Deny,
}

/// Async approval bridge provided by clients that support interactive approval.
#[async_trait]
pub trait ToolApprovalRequester: Send + Sync {
    async fn request_approval(&self, request: ToolApprovalRequest) -> ToolApprovalDecision;
}

tokio::task_local! {
    /// Optional task-local approval bridge scoped around a turn by interactive clients.
    pub static TOOL_APPROVAL_CTX: Arc<dyn ToolApprovalRequester>;
}

/// Request emitted by the `ask_user_question` tool when it asks the user a
/// structured multiple-choice question mid-turn (UPCR-2026-023).
///
/// Mirrors [`ToolApprovalRequest`]: a typed payload the
/// [`UserQuestionRequester`] surfaces to the attached client, blocking the
/// tool on a oneshot until the client answers. `questions` is already
/// validated (1..=4 questions, 2..=4 options each); `title`/`body` are the
/// mandatory generic fallback text a non-structured client renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserQuestionRequest {
    pub questions: Vec<octos_core::ui_protocol::UserQuestion>,
    pub title: String,
    pub body: String,
}

/// Outcome returned to the blocked `ask_user_question` tool after client
/// handling (UPCR-2026-023). Mirrors [`ToolApprovalDecision`] but carries the
/// structured per-question answers and distinguishes a cancelled turn from an
/// unsupported client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserQuestionOutcome {
    /// The client answered; one entry per question, in question order.
    Answered(Vec<octos_core::ui_protocol::UserQuestionAnswer>),
    /// The turn was interrupted / the pending question drained before an
    /// answer arrived. The tool returns a cancelled result.
    Cancelled,
    /// No capable client was attached for this turn; the tool degrades to the
    /// structured-metadata fallback (§4.4).
    Unsupported,
}

/// Async user-question bridge provided by clients that negotiated
/// `user_question.v1` (UPCR-2026-023). Mirrors [`ToolApprovalRequester`]:
/// scoped per-turn via [`USER_QUESTION_CTX`] so the `ask_user_question` tool
/// can block on a oneshot until `user_question/respond` resolves it.
#[async_trait]
pub trait UserQuestionRequester: Send + Sync {
    async fn request_user_question(&self, request: UserQuestionRequest) -> UserQuestionOutcome;
}

tokio::task_local! {
    /// Optional task-local user-question bridge scoped around a turn by
    /// interactive clients that negotiated `user_question.v1`. When unset the
    /// `ask_user_question` tool degrades gracefully (no hard block).
    pub static USER_QUESTION_CTX: Arc<dyn UserQuestionRequester>;
}
