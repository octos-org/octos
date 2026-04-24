//! Session-scoped background task inspection.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use serde::Deserialize;
use serde_json::json;

use crate::task_supervisor::TaskSupervisor;

use super::{Tool, ToolResult};

/// Tool that exposes background task supervisor state for the current session.
pub struct CheckBackgroundTasksTool {
    supervisor: Arc<TaskSupervisor>,
    session_key: String,
}

impl CheckBackgroundTasksTool {
    pub fn new(supervisor: Arc<TaskSupervisor>, session_key: impl Into<String>) -> Self {
        Self {
            supervisor,
            session_key: session_key.into(),
        }
    }
}

#[derive(Deserialize)]
struct Input {
    #[serde(default = "default_include_completed")]
    include_completed: bool,
}

fn default_include_completed() -> bool {
    true
}

#[async_trait]
impl Tool for CheckBackgroundTasksTool {
    fn name(&self) -> &str {
        "check_background_tasks"
    }

    fn description(&self) -> &str {
        "Inspect background task state for the current session only. Use this when the user asks whether a background job like TTS, slides, podcast, or other spawned work is done yet."
    }

    fn concurrency_class(&self) -> super::ConcurrencyClass {
        // Item 6 of OCTOS_M8_FIX_FIRST_CHECKLIST_2026-04-24:
        // check_background_tasks reads the shared TaskSupervisor state.
        // The reads are atomic individually but a sibling tool that
        // mutates supervisor state in the same batch (e.g. spawn) can
        // produce inconsistent snapshots. Mark Exclusive so the
        // supervisor view is taken at a single point in batch order.
        super::ConcurrencyClass::Exclusive
    }

    fn tags(&self) -> &[&str] {
        &["gateway"]
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "include_completed": {
                    "type": "boolean",
                    "description": "Whether to include completed and failed tasks. Defaults to true."
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: Input = serde_json::from_value(args.clone())
            .wrap_err("invalid check_background_tasks input")?;
        let mut tasks = self.supervisor.get_tasks_for_session(&self.session_key);
        tasks.sort_by(|left, right| {
            right
                .started_at
                .cmp(&left.started_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        if !input.include_completed {
            tasks.retain(|task| task.status.is_active());
        }

        let output = json!({
            "session_key": self.session_key,
            "active_count": tasks.iter().filter(|task| task.status.is_active()).count(),
            "tasks": tasks,
        });

        Ok(ToolResult {
            output: serde_json::to_string_pretty(&output).unwrap_or_else(|_| output.to_string()),
            success: true,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_supervisor::TaskStatus;

    #[tokio::test]
    async fn returns_tasks_for_current_session_only() {
        let supervisor = Arc::new(TaskSupervisor::new());
        let current = supervisor.register("fm_tts", "call-1", Some("api:sess-1"));
        let other = supervisor.register("fm_tts", "call-2", Some("api:sess-2"));
        supervisor.mark_running(&current);
        supervisor.mark_completed(&other, vec!["/tmp/out.mp3".to_string()]);

        let tool = CheckBackgroundTasksTool::new(supervisor, "api:sess-1");
        let result = tool.execute(&json!({})).await.unwrap();
        assert!(result.success);

        let payload: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        let tasks = payload["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["tool_name"], "fm_tts");
        assert_eq!(tasks[0]["status"], "running");
        assert_eq!(tasks[0]["runtime_state"], "executing_tool");
    }

    #[tokio::test]
    async fn can_filter_completed_tasks() {
        let supervisor = Arc::new(TaskSupervisor::new());
        let active = supervisor.register("fm_tts", "call-1", Some("api:sess-1"));
        let done = supervisor.register("podcast_generate", "call-2", Some("api:sess-1"));
        supervisor.mark_running(&active);
        supervisor.mark_completed(&done, vec!["/tmp/podcast.mp3".to_string()]);

        let tool = CheckBackgroundTasksTool::new(supervisor, "api:sess-1");
        let result = tool
            .execute(&json!({"include_completed": false}))
            .await
            .unwrap();

        let payload: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        let tasks = payload["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["status"], TaskStatus::Running.as_str());
        assert_eq!(tasks[0]["runtime_state"], "executing_tool");
    }
}
