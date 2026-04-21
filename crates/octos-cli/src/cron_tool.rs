//! Cron tool for scheduling tasks via the agent.
//!
//! Lives in octos-cli (not octos-agent) to avoid a octos-agent -> octos-bus dependency.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use octos_agent::tools::{Tool, ToolResult};
use octos_bus::{CronPayload, CronSchedule, CronService};
use serde::Deserialize;

pub struct CronTool {
    service: Arc<CronService>,
    default_channel: std::sync::Mutex<String>,
    default_chat_id: std::sync::Mutex<String>,
}

impl CronTool {
    pub fn new(service: Arc<CronService>) -> Self {
        Self {
            service,
            default_channel: std::sync::Mutex::new(String::new()),
            default_chat_id: std::sync::Mutex::new(String::new()),
        }
    }

    /// Create a new CronTool with context pre-set (for per-session instances).
    pub fn with_context(
        service: Arc<CronService>,
        channel: impl Into<String>,
        chat_id: impl Into<String>,
    ) -> Self {
        Self {
            service,
            default_channel: std::sync::Mutex::new(channel.into()),
            default_chat_id: std::sync::Mutex::new(chat_id.into()),
        }
    }

    /// Update the default channel/chat_id context (called per inbound message).
    pub fn set_context(&self, channel: &str, chat_id: &str) {
        *self
            .default_channel
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = channel.to_string();
        *self
            .default_chat_id
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = chat_id.to_string();
    }
}

#[derive(Deserialize)]
struct Input {
    action: String,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    every_seconds: Option<i64>,
    #[serde(default)]
    after_seconds: Option<i64>,
    #[serde(default)]
    cron_expr: Option<String>,
    #[serde(default)]
    at_ms: Option<i64>,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    chat_id: Option<String>,
    #[serde(default)]
    job_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    timezone: Option<String>,
}

#[async_trait]
impl Tool for CronTool {
    fn name(&self) -> &str {
        "cron"
    }

    fn description(&self) -> &str {
        "Schedule recurring or one-time tasks. Actions: add, list, remove, enable, disable. \
         The 'message' is an instruction sent to you (the agent) when the job fires — you will \
         process it through your full tool chain (call tools, check data, reason about results). \
         This means you can schedule complex tasks like 'Check system metrics and report only \
         if CPU > 80% or memory > 90%' — the message is your task, not the final output. \
         Respond with [SILENT] to suppress delivery when no action is needed. \
         When adding a job, 'channel' and 'chat_id' are auto-filled from the current \
         conversation — you do NOT need to ask the user for them. Just call add with \
         'message' and 'every_seconds' (or 'cron_expr'). \
         IMPORTANT: cron expressions are evaluated in UTC by default. Use the 'timezone' \
         parameter (IANA name like 'America/Los_Angeles', 'Asia/Shanghai') so the user's \
         local time is interpreted correctly. Always set timezone when the user specifies \
         a local time. \
         For relative one-time reminders (e.g. 'in 10 minutes'), prefer 'after_seconds' \
         to avoid timestamp math errors. \
         Use 'every_seconds' for recurring reminders, not 'at_ms'. \
         To remove jobs, use 'name' for fuzzy matching (preferred) or 'job_id' for exact match."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["add", "list", "remove", "enable", "disable"],
                    "description": "The action to perform"
                },
                "message": {
                    "type": "string",
                    "description": "Instruction for the agent to process when the job fires. This is NOT sent directly to the user — instead, you (the agent) receive it as a task, execute tools as needed, and compose a response. Respond with [SILENT] to suppress output. Required for 'add'."
                },
                "every_seconds": {
                    "type": "integer",
                    "description": "Interval in seconds for recurring jobs"
                },
                "after_seconds": {
                    "type": "integer",
                    "description": "One-time delay from now in seconds (preferred for relative reminders like 'in 10 minutes')"
                },
                "cron_expr": {
                    "type": "string",
                    "description": "Cron expression for schedule (e.g. '0 0 9 * * * *' for daily at 9am)"
                },
                "at_ms": {
                    "type": "integer",
                    "description": "One-time run at this Unix timestamp in milliseconds"
                },
                "name": {
                    "type": "string",
                    "description": "Name for the job. For 'add': optional label. For 'remove': matches jobs by name (partial, case-insensitive)."
                },
                "channel": {
                    "type": "string",
                    "description": "Channel to deliver to: 'telegram', 'whatsapp', 'feishu', etc. Must match the current conversation's channel."
                },
                "chat_id": {
                    "type": "string",
                    "description": "Chat ID to deliver to. Use the current conversation's chat_id / sender_id."
                },
                "job_id": {
                    "type": "string",
                    "description": "Job ID for 'remove' (or use 'name' to match by name)"
                },
                "timezone": {
                    "type": "string",
                    "description": "IANA timezone for cron_expr (e.g. 'America/Los_Angeles', 'Asia/Shanghai', 'Europe/London'). Cron expressions are in UTC by default — always set this when the user specifies a local time."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: Input =
            serde_json::from_value(args.clone()).wrap_err("invalid cron tool input")?;

        match input.action.as_str() {
            "add" => self.handle_add(input),
            "list" => Ok(self.handle_list()),
            "remove" => Ok(self.handle_remove(input)),
            "enable" => Ok(self.handle_enable(input, true)),
            "disable" => Ok(self.handle_enable(input, false)),
            other => Ok(ToolResult {
                output: format!(
                    "Unknown action: {other}. Use 'add', 'list', 'remove', 'enable', or 'disable'."
                ),
                success: false,
                ..Default::default()
            }),
        }
    }
}

impl CronTool {
    fn handle_add(&self, input: Input) -> Result<ToolResult> {
        let message = match input.message {
            Some(m) => m,
            None => {
                return Ok(ToolResult {
                    output: "'message' is required for 'add' action.".into(),
                    success: false,
                    ..Default::default()
                });
            }
        };

        let (schedule, desc) = if let Some(s) = input.every_seconds {
            if s <= 0 {
                return Ok(ToolResult {
                    output: "'every_seconds' must be a positive integer.".into(),
                    success: false,
                    ..Default::default()
                });
            }
            (
                CronSchedule::Every { every_ms: s * 1000 },
                format!("every {s}s"),
            )
        } else if let Some(s) = input.after_seconds {
            if s <= 0 {
                return Ok(ToolResult {
                    output: "'after_seconds' must be a positive integer.".into(),
                    success: false,
                    ..Default::default()
                });
            }
            let now_ms = chrono::Utc::now().timestamp_millis();
            let at_ms = now_ms.saturating_add(s.saturating_mul(1000));
            (
                CronSchedule::At { at_ms },
                format!("once in {s}s (at {at_ms})"),
            )
        } else if let Some(expr) = input.cron_expr {
            (
                CronSchedule::Cron { expr: expr.clone() },
                format!("cron: {expr}"),
            )
        } else if let Some(at) = input.at_ms {
            let now_ms = chrono::Utc::now().timestamp_millis();
            if at <= now_ms {
                return Ok(ToolResult {
                    output: "'at_ms' must be a future Unix timestamp in milliseconds. For relative reminders, use 'after_seconds'.".into(),
                    success: false,
                    ..Default::default()
                });
            }
            (CronSchedule::At { at_ms: at }, format!("once at {at}"))
        } else {
            return Ok(ToolResult {
                output: "One of 'every_seconds', 'after_seconds', 'cron_expr', or 'at_ms' is required for 'add'."
                    .into(),
                success: false,
                ..Default::default()
            });
        };

        // Auto-fill channel/chat_id from current session context if not provided
        let channel = input.channel.or_else(|| {
            let ch = self
                .default_channel
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if ch.is_empty() {
                None
            } else {
                Some(ch.clone())
            }
        });
        let chat_id = input.chat_id.or_else(|| {
            let cid = self
                .default_chat_id
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if cid.is_empty() {
                None
            } else {
                Some(cid.clone())
            }
        });

        let payload = CronPayload {
            message,
            deliver: channel.is_some(),
            channel,
            chat_id,
        };

        let name = input.name.unwrap_or_else(|| "unnamed".into());
        let job = self
            .service
            .add_job_with_tz(name, schedule, payload, input.timezone)?;

        Ok(ToolResult {
            output: format!("Created job '{}' (id: {}), {desc}.", job.name, job.id),
            success: true,
            ..Default::default()
        })
    }

    fn handle_list(&self) -> ToolResult {
        let jobs = self.service.list_jobs();
        if jobs.is_empty() {
            return ToolResult {
                output: "No scheduled jobs.".into(),
                success: true,
                ..Default::default()
            };
        }

        let mut out = format!("{} scheduled job(s):\n\n", jobs.len());
        for (i, job) in jobs.iter().enumerate() {
            let schedule_desc = match &job.schedule {
                CronSchedule::At { at_ms } => format!("once at {at_ms}"),
                CronSchedule::Every { every_ms } => format!("every {}s", every_ms / 1000),
                CronSchedule::Cron { expr } => format!("cron: {expr}"),
            };
            let timezone = job.timezone.as_deref().unwrap_or("UTC(default)");
            out.push_str(&format!(
                "{}. [{}] {} — {} [tz: {}] (msg: \"{}\")\n",
                i + 1,
                job.id,
                job.name,
                schedule_desc,
                timezone,
                truncate(&job.payload.message, 60),
            ));
        }

        ToolResult {
            output: out,
            success: true,
            ..Default::default()
        }
    }

    fn handle_remove(&self, input: Input) -> ToolResult {
        // Try job_id first, then fall back to name matching
        if let Some(id) = &input.job_id {
            if self.service.remove_job(id) {
                return ToolResult {
                    output: format!("Removed job {id}."),
                    success: true,
                    ..Default::default()
                };
            }
            return ToolResult {
                output: format!("Job {id} not found."),
                success: false,
                ..Default::default()
            };
        }

        // Match by name (case-insensitive, partial match)
        if let Some(name) = &input.name {
            let query = name.to_lowercase();
            let matching: Vec<String> = self
                .service
                .list_jobs()
                .iter()
                .filter(|j| {
                    j.name.to_lowercase().contains(&query)
                        || j.payload.message.to_lowercase().contains(&query)
                })
                .map(|j| j.id.clone())
                .collect();

            if matching.is_empty() {
                return ToolResult {
                    output: format!("No jobs matching '{name}'."),
                    success: false,
                    ..Default::default()
                };
            }

            let mut removed = Vec::new();
            for id in &matching {
                if self.service.remove_job(id) {
                    removed.push(id.clone());
                }
            }

            return ToolResult {
                output: format!("Removed {} job(s): {}", removed.len(), removed.join(", ")),
                success: true,
                ..Default::default()
            };
        }

        ToolResult {
            output: "'job_id' or 'name' is required for 'remove' action.".into(),
            success: false,
            ..Default::default()
        }
    }

    fn handle_enable(&self, input: Input, enabled: bool) -> ToolResult {
        let id = match input.job_id {
            Some(id) => id,
            None => {
                return ToolResult {
                    output: "'job_id' is required for enable/disable action.".into(),
                    success: false,
                    ..Default::default()
                };
            }
        };

        let action = if enabled { "Enabled" } else { "Disabled" };
        if self.service.enable_job(&id, enabled) {
            ToolResult {
                output: format!("{action} job {id}."),
                success: true,
                ..Default::default()
            }
        } else {
            ToolResult {
                output: format!("Job {id} not found."),
                success: false,
                ..Default::default()
            }
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let end: String = s.chars().take(max).collect();
        format!("{end}...")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    fn make_service(
        dir: &std::path::Path,
    ) -> (Arc<CronService>, mpsc::Receiver<octos_core::InboundMessage>) {
        let (tx, rx) = mpsc::channel(64);
        let service = Arc::new(CronService::new(dir.join("cron.json"), tx));
        (service, rx)
    }

    #[tokio::test]
    async fn test_list_empty() {
        let dir = tempfile::tempdir().unwrap();
        let (service, _rx) = make_service(dir.path());
        let tool = CronTool::new(service);

        let result = tool
            .execute(&serde_json::json!({"action": "list"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("No scheduled"));
    }

    #[tokio::test]
    async fn test_add_and_list() {
        let dir = tempfile::tempdir().unwrap();
        let (service, _rx) = make_service(dir.path());
        let tool = CronTool::new(service);

        let result = tool
            .execute(&serde_json::json!({
                "action": "add",
                "message": "check status",
                "every_seconds": 300,
                "name": "status-check"
            }))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("status-check"));

        let list = tool
            .execute(&serde_json::json!({"action": "list"}))
            .await
            .unwrap();
        assert!(list.success);
        assert!(list.output.contains("status-check"));
        assert!(list.output.contains("every 300s"));
        assert!(list.output.contains("UTC(default)"));
    }

    #[tokio::test]
    async fn test_add_and_remove() {
        let dir = tempfile::tempdir().unwrap();
        let (service, _rx) = make_service(dir.path());
        let tool = CronTool::new(service);

        let add_result = tool
            .execute(&serde_json::json!({
                "action": "add",
                "message": "temp",
                "every_seconds": 60
            }))
            .await
            .unwrap();
        assert!(add_result.success);

        // Extract job ID from output
        let id = add_result
            .output
            .split("id: ")
            .nth(1)
            .unwrap()
            .split(')')
            .next()
            .unwrap();

        let remove = tool
            .execute(&serde_json::json!({"action": "remove", "job_id": id}))
            .await
            .unwrap();
        assert!(remove.success);
        assert!(remove.output.contains("Removed"));
    }

    #[tokio::test]
    async fn test_add_after_seconds_uses_current_time() {
        let dir = tempfile::tempdir().unwrap();
        let (service, _rx) = make_service(dir.path());
        let tool = CronTool::new(service.clone());

        let before = chrono::Utc::now().timestamp_millis();
        let add = tool
            .execute(&serde_json::json!({
                "action": "add",
                "message": "drink water",
                "after_seconds": 600,
                "name": "relative"
            }))
            .await
            .unwrap();
        let after = chrono::Utc::now().timestamp_millis();
        assert!(add.success);

        let jobs = service.list_jobs();
        assert_eq!(jobs.len(), 1);
        let at_ms = match jobs[0].schedule {
            CronSchedule::At { at_ms } => at_ms,
            _ => panic!("expected one-time schedule"),
        };

        let lower = before + 600_000;
        let upper = after + 600_000 + 2_000;
        assert!(
            at_ms >= lower && at_ms <= upper,
            "at_ms out of range: {at_ms}, expected [{lower}, {upper}]"
        );
    }

    #[tokio::test]
    async fn test_add_rejects_past_at_ms() {
        let dir = tempfile::tempdir().unwrap();
        let (service, _rx) = make_service(dir.path());
        let tool = CronTool::new(service);

        let past = chrono::Utc::now().timestamp_millis() - 1;
        let add = tool
            .execute(&serde_json::json!({
                "action": "add",
                "message": "past",
                "at_ms": past
            }))
            .await
            .unwrap();
        assert!(!add.success);
        assert!(add.output.contains("future Unix timestamp"));
    }
}
