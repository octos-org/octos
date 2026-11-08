//! Stable app-facing UI API for octos clients.
//!
//! This layer is intentionally above the draft JSON-RPC wire protocol. TUI and
//! future app surfaces should depend on these app concepts, while transports can
//! translate to `ui_protocol` or another backend implementation underneath.

use serde::{Deserialize, Serialize};

use crate::ui_protocol::{
    self, ApprovalRespondParams, ApprovalScopesListParams, DiffPreviewGetParams, SessionOpenParams,
    TaskCancelParams, TaskListParams, TaskOutputReadParams, TaskRestartFromNodeParams,
    TaskRuntimeState, TurnId, TurnInterruptParams, TurnStartParams, UiCommand, UiNotification,
    UiProgressEvent, methods,
};
use crate::{Message, SessionKey, TaskId};

/// Stable app UI API identifier.
pub const APP_UI_API_V1: &str = "octos-app-ui/v1alpha1";

pub type AppUiInputItem = ui_protocol::InputItem;
pub type AppUiOpenSession = SessionOpenParams;
pub type AppUiSubmitPrompt = TurnStartParams;
pub type AppUiInterruptTurn = TurnInterruptParams;
pub type AppUiRespondApproval = ApprovalRespondParams;
pub type AppUiListApprovalScopes = ApprovalScopesListParams;
pub type AppUiGetDiffPreview = DiffPreviewGetParams;
pub type AppUiListTasks = TaskListParams;
pub type AppUiCancelTask = TaskCancelParams;
pub type AppUiRestartTaskFromNode = TaskRestartFromNodeParams;
pub type AppUiReadTaskOutput = TaskOutputReadParams;
pub type AppUiBackendEvent = UiNotification;
pub type AppUiProgress = UiProgressEvent;
pub type AppUiCommandResult = ui_protocol::UiRpcResult;
pub type AppUiCommandResultKind = ui_protocol::UiResultKind;
pub type AppUiSessionOpened = ui_protocol::SessionOpened;
pub type AppUiTurnStarted = ui_protocol::TurnStartedEvent;
pub type AppUiTurnCompleted = ui_protocol::TurnCompletedEvent;
pub type AppUiTurnError = ui_protocol::TurnErrorEvent;
pub type AppUiMessageDelta = ui_protocol::MessageDeltaEvent;
pub type AppUiTaskUpdated = ui_protocol::TaskUpdatedEvent;
pub type AppUiTaskOutputDelta = ui_protocol::TaskOutputDeltaEvent;
pub type AppUiWarning = ui_protocol::WarningEvent;

/// Launch-time client preferences for an app UI backend.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppUiLaunch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionKey>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,
    pub readonly: bool,
}

/// Snapshot used to hydrate app UI state before event streaming begins.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppUiSnapshot {
    pub sessions: Vec<AppUiSession>,
    pub selected_session: usize,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub readonly: bool,
}

/// Client-ready session view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppUiSession {
    pub id: SessionKey,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    pub messages: Vec<Message>,
    pub tasks: Vec<AppUiTask>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_reply: Option<AppUiLiveReply>,
}

/// Client-ready task view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppUiTask {
    pub id: TaskId,
    pub title: String,
    pub state: TaskRuntimeState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_detail: Option<String>,
    pub output_tail: String,
    /// C1 step 4: the turn that originated this task. Lets the client
    /// attribute the task (and its terminal transition) to a specific turn
    /// so the "N running" task count can be reconciled per-turn instead of
    /// staying stuck when a sub-agent fails/recovers/is-orphaned. Optional
    /// so legacy daemons and synthetic emitters that cannot resolve the
    /// originating turn still parse; omitted from the wire when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
}

/// In-flight assistant response rendered by app UIs before it is committed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppUiLiveReply {
    pub turn_id: TurnId,
    pub text: String,
}

/// Stable app command surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AppUiCommand {
    OpenSession(AppUiOpenSession),
    SubmitPrompt(AppUiSubmitPrompt),
    InterruptTurn(AppUiInterruptTurn),
    RespondApproval(AppUiRespondApproval),
    ListApprovalScopes(AppUiListApprovalScopes),
    GetDiffPreview(AppUiGetDiffPreview),
    ListTasks(AppUiListTasks),
    CancelTask(AppUiCancelTask),
    RestartTaskFromNode(AppUiRestartTaskFromNode),
    ReadTaskOutput(AppUiReadTaskOutput),
}

impl AppUiCommand {
    pub fn method(&self) -> &'static str {
        match self {
            Self::OpenSession(_) => methods::SESSION_OPEN,
            Self::SubmitPrompt(_) => methods::TURN_START,
            Self::InterruptTurn(_) => methods::TURN_INTERRUPT,
            Self::RespondApproval(_) => methods::APPROVAL_RESPOND,
            Self::ListApprovalScopes(_) => methods::APPROVAL_SCOPES_LIST,
            Self::GetDiffPreview(_) => methods::DIFF_PREVIEW_GET,
            Self::ListTasks(_) => methods::TASK_LIST,
            Self::CancelTask(_) => methods::TASK_CANCEL,
            Self::RestartTaskFromNode(_) => methods::TASK_RESTART_FROM_NODE,
            Self::ReadTaskOutput(_) => methods::TASK_OUTPUT_READ,
        }
    }

    pub fn first_server_result_kind(&self) -> Option<AppUiCommandResultKind> {
        ui_protocol::first_server_result_kind_for_method(self.method())
    }

    pub fn into_protocol(self) -> UiCommand {
        match self {
            Self::OpenSession(params) => UiCommand::SessionOpen(params),
            Self::SubmitPrompt(params) => UiCommand::TurnStart(params),
            Self::InterruptTurn(params) => UiCommand::TurnInterrupt(params),
            Self::RespondApproval(params) => UiCommand::ApprovalRespond(params),
            Self::ListApprovalScopes(params) => UiCommand::ApprovalScopesList(params),
            Self::GetDiffPreview(params) => UiCommand::DiffPreviewGet(params),
            Self::ListTasks(params) => UiCommand::TaskList(params),
            Self::CancelTask(params) => UiCommand::TaskCancel(params),
            Self::RestartTaskFromNode(params) => UiCommand::TaskRestartFromNode(params),
            Self::ReadTaskOutput(params) => UiCommand::TaskOutputRead(params),
        }
    }
}

impl From<AppUiCommand> for UiCommand {
    fn from(command: AppUiCommand) -> Self {
        command.into_protocol()
    }
}

/// App events emitted by a backend implementation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum AppUiEvent {
    Snapshot(AppUiSnapshot),
    Protocol(AppUiBackendEvent),
    Progress(AppUiProgress),
    Status(AppUiStatus),
    Error(AppUiError),
}

impl AppUiEvent {
    pub fn from_protocol(notification: AppUiBackendEvent) -> Self {
        Self::Protocol(notification)
    }

    pub fn as_protocol(&self) -> Option<&AppUiBackendEvent> {
        match self {
            Self::Protocol(notification) => Some(notification),
            _ => None,
        }
    }

    pub fn into_protocol(self) -> Option<AppUiBackendEvent> {
        match self {
            Self::Protocol(notification) => Some(notification),
            _ => None,
        }
    }

    pub fn from_progress(progress: AppUiProgress) -> Self {
        Self::Progress(progress)
    }

    pub fn as_progress(&self) -> Option<&AppUiProgress> {
        match self {
            Self::Progress(progress) => Some(progress),
            _ => None,
        }
    }

    pub fn into_progress(self) -> Option<AppUiProgress> {
        match self {
            Self::Progress(progress) => Some(progress),
            _ => None,
        }
    }

    pub fn protocol_method(&self) -> Option<&'static str> {
        match self {
            Self::Protocol(notification) => Some(notification.method()),
            Self::Progress(progress) => Some(progress.method()),
            Self::Snapshot(_) | Self::Status(_) | Self::Error(_) => None,
        }
    }

    pub fn status(message: impl Into<String>) -> Self {
        Self::Status(AppUiStatus {
            message: message.into(),
        })
    }

    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Error(AppUiError {
            code: code.into(),
            message: message.into(),
        })
    }
}

impl From<AppUiBackendEvent> for AppUiEvent {
    fn from(notification: AppUiBackendEvent) -> Self {
        Self::from_protocol(notification)
    }
}

impl From<AppUiProgress> for AppUiEvent {
    fn from(progress: AppUiProgress) -> Self {
        Self::from_progress(progress)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppUiStatus {
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppUiError {
    pub code: String,
    pub message: String,
}
