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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_command_method_maps_to_protocol_method() {
        let command = AppUiCommand::SubmitPrompt(TurnStartParams {
            session_id: SessionKey("local:test".into()),
            turn_id: TurnId::new(),
            input: vec![AppUiInputItem::Text {
                text: "hello".into(),
            }],
        });

        assert_eq!(command.method(), methods::TURN_START);
        assert_eq!(
            command.first_server_result_kind(),
            Some(AppUiCommandResultKind::TurnStart)
        );
        assert_eq!(UiCommand::from(command).method(), methods::TURN_START);
    }

    #[test]
    fn app_command_surface_covers_approval_scopes_list() {
        let command = AppUiCommand::ListApprovalScopes(AppUiListApprovalScopes {
            session_id: SessionKey("local:test".into()),
        });

        assert_eq!(command.method(), methods::APPROVAL_SCOPES_LIST);
        assert_eq!(
            command.first_server_result_kind(),
            Some(AppUiCommandResultKind::ApprovalScopesList)
        );
        assert_eq!(
            UiCommand::from(command).method(),
            methods::APPROVAL_SCOPES_LIST
        );
    }

    #[test]
    fn app_command_surface_covers_harness_task_control() {
        let session_id = SessionKey("local:test".into());
        let task_id = TaskId::new();

        let list = AppUiCommand::ListTasks(AppUiListTasks {
            session_id: session_id.clone(),
            topic: Some("default".into()),
        });
        assert_eq!(list.method(), methods::TASK_LIST);
        assert_eq!(
            list.first_server_result_kind(),
            Some(AppUiCommandResultKind::TaskList)
        );
        assert_eq!(UiCommand::from(list).method(), methods::TASK_LIST);

        let cancel = AppUiCommand::CancelTask(AppUiCancelTask {
            task_id: task_id.clone(),
            session_id: Some(session_id.clone()),
            profile_id: Some("coding".into()),
        });
        assert_eq!(cancel.method(), methods::TASK_CANCEL);
        assert_eq!(
            cancel.first_server_result_kind(),
            Some(AppUiCommandResultKind::TaskCancel)
        );
        assert_eq!(UiCommand::from(cancel).method(), methods::TASK_CANCEL);

        let restart = AppUiCommand::RestartTaskFromNode(AppUiRestartTaskFromNode {
            task_id,
            node_id: Some("node-1".into()),
            session_id: Some(session_id),
            profile_id: None,
        });
        assert_eq!(restart.method(), methods::TASK_RESTART_FROM_NODE);
        assert_eq!(
            restart.first_server_result_kind(),
            Some(AppUiCommandResultKind::TaskRestartFromNode)
        );
        assert_eq!(
            UiCommand::from(restart).method(),
            methods::TASK_RESTART_FROM_NODE
        );
    }

    #[test]
    fn app_event_helpers_expose_protocol_without_variant_matching() {
        let event = AppUiEvent::from_protocol(UiNotification::Warning(AppUiWarning {
            session_id: SessionKey("local:test".into()),
            turn_id: None,
            code: "mock_warning".into(),
            message: "hello".into(),
        }));

        assert_eq!(event.protocol_method(), Some(methods::WARNING));
        assert!(event.as_protocol().is_some());

        let protocol = event.into_protocol().expect("protocol event");
        assert_eq!(protocol.method(), methods::WARNING);

        let status = AppUiEvent::status("ready");
        assert!(status.as_protocol().is_none());

        let error = AppUiEvent::error("bad_frame", "frame was invalid");
        let AppUiEvent::Error(error) = error else {
            panic!("expected app UI error");
        };
        assert_eq!(error.code, "bad_frame");
    }

    #[test]
    fn app_event_helpers_expose_progress_without_raw_protocol_matching() {
        let progress = AppUiProgress::new(
            SessionKey("local:test".into()),
            None,
            ui_protocol::UiProgressMetadata::new(ui_protocol::progress_kinds::STATUS)
                .with_message("working"),
        );
        let event = AppUiEvent::from_progress(progress.clone());

        assert_eq!(event.protocol_method(), Some(methods::PROGRESS_UPDATED));
        assert_eq!(event.as_progress(), Some(&progress));
        assert!(event.as_protocol().is_none());

        let decoded = event.into_progress().expect("progress event");
        assert_eq!(decoded.method(), methods::PROGRESS_UPDATED);
    }

    #[test]
    fn snapshot_round_trips_through_json() {
        let snapshot = AppUiSnapshot {
            sessions: vec![AppUiSession {
                id: SessionKey("coding:local:prototype#m9".into()),
                title: "M9 protocol draft".into(),
                profile_id: Some("coding".into()),
                messages: vec![Message::system("mock bootstrap")],
                tasks: vec![AppUiTask {
                    id: TaskId::new(),
                    title: "protocol spike".into(),
                    state: TaskRuntimeState::Running,
                    runtime_detail: Some("seeded".into()),
                    output_tail: "bootstrap\n".into(),
                }],
                live_reply: None,
            }],
            selected_session: 0,
            status: "ready".into(),
            target: Some("local mock snapshot".into()),
            readonly: true,
        };

        let json = serde_json::to_string(&snapshot).expect("serialize snapshot");
        let decoded: AppUiSnapshot = serde_json::from_str(&json).expect("deserialize snapshot");

        assert_eq!(decoded.sessions[0].id.0, "coding:local:prototype#m9");
        assert_eq!(decoded.sessions[0].tasks[0].title, "protocol spike");
        assert!(decoded.readonly);
    }

    #[test]
    fn protocol_event_round_trips_through_json() {
        let event =
            AppUiEvent::Protocol(UiNotification::Warning(crate::ui_protocol::WarningEvent {
                session_id: SessionKey("local:test".into()),
                turn_id: None,
                code: "mock_warning".into(),
                message: "hello".into(),
            }));

        let json = serde_json::to_string(&event).expect("serialize event");
        let decoded: AppUiEvent = serde_json::from_str(&json).expect("deserialize event");

        let AppUiEvent::Protocol(UiNotification::Warning(warning)) = decoded else {
            panic!("expected protocol warning event");
        };
        assert_eq!(warning.code, "mock_warning");
    }
}
