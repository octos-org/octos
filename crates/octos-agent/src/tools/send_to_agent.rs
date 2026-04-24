//! `send_to_agent` tool — deliver a steering message to a running sub-agent.
//!
//! Closes demo step 7 of the PM-subagents audit: "PM agent steers sub-agent
//! 1 with follow-up guidance while it is still running". The tool looks
//! up the target task in [`TaskSupervisor`] and pushes the caller's message
//! into its bound [`SupervisorInbox`].
//!
//! Semantics:
//! - Returns `success=true` when the inbox accepts the message.
//! - Returns `success=false` with a structured error kind (`unknown_task`,
//!   `no_inbox`, `inbox_closed`, `terminal`) when the target cannot receive.
//! - Non-blocking: the supervisor does not wait for the sub-agent to ack
//!   the message. Delivery is best-effort; sub-agents that are suspended
//!   waiting for an LLM response will drain the inbox on the next loop turn.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use serde::Deserialize;
use serde_json::json;

use crate::task_supervisor::{InboxMessage, SendToAgentError, TaskSupervisor};

use super::{Tool, ToolResult};

/// Tool that delivers a free-form steering message into a running
/// sub-agent's inbox.
pub struct SendToAgentTool {
    supervisor: Arc<TaskSupervisor>,
    /// Stable sender label (usually the parent session key or an operator
    /// id). Attached to each inbox entry so the sub-agent loop can
    /// render attribution when prepending the message to its conversation
    /// history.
    sender_label: String,
}

impl SendToAgentTool {
    pub fn new(supervisor: Arc<TaskSupervisor>, sender_label: impl Into<String>) -> Self {
        Self {
            supervisor,
            sender_label: sender_label.into(),
        }
    }
}

#[derive(Deserialize)]
struct Input {
    /// Supervisor task id returned by `check_background_tasks` or the
    /// REST API. Matches `BackgroundTask::id`.
    task_id: String,
    /// Free-form message body. Sent as-is into the sub-agent inbox.
    message: String,
    /// Optional override for the sender label. Defaults to the tool's
    /// configured `sender_label` (usually the parent session key).
    #[serde(default)]
    sender: Option<String>,
}

#[async_trait]
impl Tool for SendToAgentTool {
    fn name(&self) -> &str {
        "send_to_agent"
    }

    fn description(&self) -> &str {
        "Steer a running background sub-agent by appending a message into its inbox. The sub-agent drains the inbox at the start of its next loop turn and treats each entry as an additional user instruction. Useful when the PM agent needs to course-correct a long-running sub-agent without killing and re-launching it."
    }

    fn tags(&self) -> &[&str] {
        &["supervisor", "gateway"]
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "Supervisor task id to steer. Match it against `check_background_tasks` output."
                },
                "message": {
                    "type": "string",
                    "description": "Free-form steering message delivered to the sub-agent."
                },
                "sender": {
                    "type": "string",
                    "description": "Optional sender label for attribution. Defaults to the parent session key."
                }
            },
            "required": ["task_id", "message"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: Input =
            serde_json::from_value(args.clone()).wrap_err("invalid send_to_agent input")?;

        let sender = input.sender.unwrap_or_else(|| self.sender_label.clone());
        let msg = InboxMessage::new(sender.clone(), input.message.clone());

        match self.supervisor.send_to_agent(&input.task_id, msg) {
            Ok(()) => {
                let payload = json!({
                    "task_id": input.task_id,
                    "sender": sender,
                    "delivered": true,
                });
                Ok(ToolResult {
                    output: payload.to_string(),
                    success: true,
                    ..Default::default()
                })
            }
            Err(err) => {
                let kind = match &err {
                    SendToAgentError::UnknownTask => "unknown_task",
                    SendToAgentError::NoInbox => "no_inbox",
                    SendToAgentError::InboxClosed => "inbox_closed",
                    SendToAgentError::Terminal(_) => "terminal",
                };
                let payload = json!({
                    "task_id": input.task_id,
                    "delivered": false,
                    "error_kind": kind,
                    "error": err.to_string(),
                });
                Ok(ToolResult {
                    output: payload.to_string(),
                    success: false,
                    ..Default::default()
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_supervisor::SupervisorInbox;

    #[tokio::test]
    async fn should_deliver_to_running_task() {
        let supervisor = Arc::new(TaskSupervisor::new());
        let id = supervisor.register("spawn", "call", Some("api:sess"));
        supervisor.mark_running(&id);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let inbox = SupervisorInbox::new(tx);
        let handle = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        });
        supervisor.register_abort(&id, handle.abort_handle(), Some(inbox), None);

        let tool = SendToAgentTool::new(supervisor, "api:sess");
        let result = tool
            .execute(&json!({
                "task_id": id,
                "message": "please switch tack"
            }))
            .await
            .unwrap();
        assert!(result.success);
        let payload: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(payload["delivered"], true);
        assert_eq!(payload["sender"], "api:sess");

        let msg = rx.try_recv().expect("inbox should have message");
        assert_eq!(msg.body, "please switch tack");
    }

    #[tokio::test]
    async fn should_surface_unknown_task_error() {
        let supervisor = Arc::new(TaskSupervisor::new());
        let tool = SendToAgentTool::new(supervisor, "api:sess");
        let result = tool
            .execute(&json!({
                "task_id": "does-not-exist",
                "message": "hi"
            }))
            .await
            .unwrap();
        assert!(!result.success);
        let payload: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(payload["delivered"], false);
        assert_eq!(payload["error_kind"], "unknown_task");
    }

    #[tokio::test]
    async fn should_surface_no_inbox_error() {
        let supervisor = Arc::new(TaskSupervisor::new());
        let id = supervisor.register("spawn", "call", Some("api:sess"));
        supervisor.mark_running(&id);
        let handle = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        });
        supervisor.register_abort(&id, handle.abort_handle(), None, None);

        let tool = SendToAgentTool::new(supervisor, "api:sess");
        let result = tool
            .execute(&json!({
                "task_id": id,
                "message": "hi"
            }))
            .await
            .unwrap();
        assert!(!result.success);
        let payload: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(payload["error_kind"], "no_inbox");
    }
}
