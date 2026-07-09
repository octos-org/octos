//! Cron tool for scheduling tasks via the agent.
//!
//! Lives in octos-cli (not octos-agent) to avoid a octos-agent -> octos-bus dependency.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{Local, Utc};
use eyre::{Result, WrapErr};
use iana_time_zone::get_timezone;
use octos_agent::tools::{Tool, ToolResult};
use octos_bus::{CronPayload, CronSchedule, CronService};
use regex::Regex;
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

    /// Create a schedule from a natural-language request, bound to a
    /// channel/chat context. Backs the Matrix `/schedule` slash command —
    /// no LLM involved, just deterministic parsing of common CJK/English
    /// schedule phrasings.
    pub fn add_natural_language_for_context(
        service: &Arc<CronService>,
        channel: &str,
        chat_id: &str,
        request: &str,
    ) -> Result<ToolResult> {
        let request = request.trim();
        if request.is_empty() {
            return Ok(ToolResult {
                output: "Usage: /schedule <natural-language task>".into(),
                success: false,
                ..Default::default()
            });
        }

        let parsed = match parse_natural_schedule_request(request) {
            Ok(parsed) => parsed,
            Err(message) => {
                return Ok(ToolResult {
                    output: message,
                    success: false,
                    ..Default::default()
                });
            }
        };

        let payload = CronPayload {
            message: parsed.message.clone(),
            deliver: true,
            channel: Some(channel.to_string()),
            chat_id: Some(chat_id.to_string()),
        };

        let job =
            service.add_job_with_tz(parsed.name, parsed.schedule, payload, parsed.timezone)?;

        Ok(ToolResult {
            output: format!(
                "Created schedule '{}' (id: {}). {}",
                job.name, job.id, parsed.description
            ),
            success: true,
            ..Default::default()
        })
    }

    /// List schedules bound to a channel/chat context (Matrix `/schedules`).
    pub fn list_jobs_for_context(
        service: &CronService,
        channel: &str,
        chat_id: &str,
    ) -> ToolResult {
        let jobs = service
            .list_all_jobs()
            .into_iter()
            .filter(|job| job_matches_context(job, channel, chat_id))
            .collect::<Vec<_>>();

        if jobs.is_empty() {
            return ToolResult {
                output: "No scheduled jobs for this chat.".into(),
                success: true,
                ..Default::default()
            };
        }

        let mut out = format!("{} scheduled job(s) for this chat:\n\n", jobs.len());
        for (i, job) in jobs.iter().enumerate() {
            out.push_str(&format!(
                "{}. [{}] {} — {} (msg: \"{}\")\n",
                i + 1,
                job.id,
                job.name,
                format_schedule_for_display(&job.schedule),
                truncate(&job.payload.message, 60),
            ));
        }

        ToolResult {
            output: out,
            success: true,
            ..Default::default()
        }
    }

    /// Remove a schedule bound to a channel/chat context (Matrix `/unschedule`).
    /// Rejects job IDs that belong to a different chat so one room cannot
    /// remove another room's schedules.
    pub fn remove_job_for_context(
        service: &Arc<CronService>,
        channel: &str,
        chat_id: &str,
        job_id: &str,
    ) -> ToolResult {
        let visible = service
            .list_all_jobs()
            .into_iter()
            .any(|job| job.id == job_id && job_matches_context(&job, channel, chat_id));

        if !visible {
            return ToolResult {
                output: format!("Job {job_id} not found in this chat."),
                success: false,
                ..Default::default()
            };
        }

        if service.remove_job(job_id) {
            ToolResult {
                output: format!("Removed job {job_id}."),
                success: true,
                ..Default::default()
            }
        } else {
            ToolResult {
                output: format!("Job {job_id} not found."),
                success: false,
                ..Default::default()
            }
        }
    }
}

struct ParsedNaturalSchedule {
    name: String,
    message: String,
    schedule: CronSchedule,
    timezone: Option<String>,
    description: String,
}

fn parse_natural_schedule_request(
    request: &str,
) -> std::result::Result<ParsedNaturalSchedule, String> {
    if let Some(parsed) = parse_delayed_schedule(request) {
        return Ok(parsed);
    }
    if let Some(parsed) = parse_interval_schedule(request) {
        return Ok(parsed);
    }
    if let Some(parsed) = parse_daily_schedule(request) {
        return Ok(parsed);
    }
    if let Some(parsed) = parse_weekly_schedule(request) {
        return Ok(parsed);
    }

    Err(
        "I couldn't understand that schedule yet. Try patterns like `20秒之后提醒我看天气`, `每天早上 9 点提醒我看天气`, `每30分钟检查状态`, or `every day at 9am remind me to check weather`.".to_string(),
    )
}

fn parse_delayed_schedule(request: &str) -> Option<ParsedNaturalSchedule> {
    let zh = Regex::new(r"^(\d+)\s*(秒|分钟|小时|天)(?:后|之后)\s*(.+)$").ok()?;
    if let Some(caps) = zh.captures(request) {
        let value = caps.get(1)?.as_str().parse::<i64>().ok()?;
        let unit = caps.get(2)?.as_str();
        let message = caps.get(3)?.as_str().trim().to_string();
        let delay_ms = interval_to_ms(value, unit)?;
        let at_ms = Utc::now().timestamp_millis() + delay_ms;
        return Some(ParsedNaturalSchedule {
            name: derive_job_name(&message),
            message,
            schedule: CronSchedule::At { at_ms },
            timezone: None,
            description: format!("Runs once in {}", interval_label(value, unit)),
        });
    }

    let en = Regex::new(
        r"(?i)^in\s+(\d+)\s+(second|seconds|minute|minutes|hour|hours|day|days)\s+(.+)$",
    )
    .ok()?;
    let caps = en.captures(request)?;
    let value = caps.get(1)?.as_str().parse::<i64>().ok()?;
    let unit = caps.get(2)?.as_str().to_ascii_lowercase();
    let message = caps.get(3)?.as_str().trim().to_string();
    let delay_ms = interval_to_ms(value, unit.as_str())?;
    let at_ms = Utc::now().timestamp_millis() + delay_ms;
    Some(ParsedNaturalSchedule {
        name: derive_job_name(&message),
        message,
        schedule: CronSchedule::At { at_ms },
        timezone: None,
        description: format!("Runs once in {value} {unit}"),
    })
}

fn parse_interval_schedule(request: &str) -> Option<ParsedNaturalSchedule> {
    let zh = Regex::new(r"^每\s*(\d+)\s*(秒|分钟|小时|天)\s*(.+)$").ok()?;
    if let Some(caps) = zh.captures(request) {
        let value = caps.get(1)?.as_str().parse::<i64>().ok()?;
        let unit = caps.get(2)?.as_str();
        let message = caps.get(3)?.as_str().trim().to_string();
        let every_seconds = interval_to_seconds(value, unit)?;
        return Some(ParsedNaturalSchedule {
            name: derive_job_name(&message),
            message,
            schedule: CronSchedule::Every {
                every_ms: every_seconds * 1000,
            },
            timezone: None,
            description: format!("Runs every {}", interval_label(value, unit)),
        });
    }

    let en = Regex::new(
        r"(?i)^every\s+(\d+)\s+(second|seconds|minute|minutes|hour|hours|day|days)\s+(.+)$",
    )
    .ok()?;
    let caps = en.captures(request)?;
    let value = caps.get(1)?.as_str().parse::<i64>().ok()?;
    let unit = caps.get(2)?.as_str().to_ascii_lowercase();
    let message = caps.get(3)?.as_str().trim().to_string();
    let every_seconds = interval_to_seconds(value, unit.as_str())?;
    Some(ParsedNaturalSchedule {
        name: derive_job_name(&message),
        message,
        schedule: CronSchedule::Every {
            every_ms: every_seconds * 1000,
        },
        timezone: None,
        description: format!("Runs every {value} {unit}"),
    })
}

fn parse_daily_schedule(request: &str) -> Option<ParsedNaturalSchedule> {
    let zh = Regex::new(
        r"^每天(?:(早上|上午|中午|下午|晚上))?\s*(\d{1,2})(?:\s*[:点时]\s*(\d{1,2})?)?\s*分?\s*(.+)$",
    )
    .ok()?;
    if let Some(caps) = zh.captures(request) {
        let qualifier = caps.get(1).map(|m| m.as_str());
        let hour = caps.get(2)?.as_str().parse::<u32>().ok()?;
        let minute = caps
            .get(3)
            .and_then(|m| m.as_str().parse::<u32>().ok())
            .unwrap_or(0);
        let message = caps.get(4)?.as_str().trim().to_string();
        let hour = apply_time_qualifier(hour, qualifier)?;
        validate_hour_minute(hour, minute)?;
        let timezone = current_local_timezone_name();
        let timezone_label = timezone.clone().unwrap_or_else(current_local_offset_label);
        return Some(ParsedNaturalSchedule {
            name: derive_job_name(&message),
            message,
            schedule: CronSchedule::Cron {
                expr: format!("0 {minute} {hour} * * * *"),
            },
            timezone,
            description: format!(
                "Runs every day at {hour:02}:{minute:02} using server local timezone {timezone_label}"
            ),
        });
    }

    let en =
        Regex::new(r"(?i)^every\s+day\s+at\s+(\d{1,2})(?::(\d{2}))?\s*(am|pm)?\s+(.+)$").ok()?;
    let caps = en.captures(request)?;
    let hour = caps.get(1)?.as_str().parse::<u32>().ok()?;
    let minute = caps
        .get(2)
        .and_then(|m| m.as_str().parse::<u32>().ok())
        .unwrap_or(0);
    let am_pm = caps.get(3).map(|m| m.as_str());
    let message = caps.get(4)?.as_str().trim().to_string();
    let hour = apply_am_pm(hour, am_pm)?;
    validate_hour_minute(hour, minute)?;
    let timezone = current_local_timezone_name();
    let timezone_label = timezone.clone().unwrap_or_else(current_local_offset_label);
    Some(ParsedNaturalSchedule {
        name: derive_job_name(&message),
        message,
        schedule: CronSchedule::Cron {
            expr: format!("0 {minute} {hour} * * * *"),
        },
        timezone,
        description: format!(
            "Runs every day at {hour:02}:{minute:02} using server local timezone {timezone_label}"
        ),
    })
}

fn parse_weekly_schedule(request: &str) -> Option<ParsedNaturalSchedule> {
    let zh = Regex::new(
        r"^每周([一二三四五六日天])(?:(早上|上午|中午|下午|晚上))?\s*(\d{1,2})(?:\s*[:点时]\s*(\d{1,2})?)?\s*分?\s*(.+)$",
    )
    .ok()?;
    if let Some(caps) = zh.captures(request) {
        let weekday = chinese_weekday_to_index(caps.get(1)?.as_str())?;
        let qualifier = caps.get(2).map(|m| m.as_str());
        let hour = caps.get(3)?.as_str().parse::<u32>().ok()?;
        let minute = caps
            .get(4)
            .and_then(|m| m.as_str().parse::<u32>().ok())
            .unwrap_or(0);
        let message = caps.get(5)?.as_str().trim().to_string();
        let hour = apply_time_qualifier(hour, qualifier)?;
        validate_hour_minute(hour, minute)?;
        let cron_weekday = cron_weekday(weekday);
        let timezone = current_local_timezone_name();
        let timezone_label = timezone.clone().unwrap_or_else(current_local_offset_label);
        return Some(ParsedNaturalSchedule {
            name: derive_job_name(&message),
            message,
            schedule: CronSchedule::Cron {
                expr: format!("0 {minute} {hour} * * {cron_weekday} *"),
            },
            timezone,
            description: format!(
                "Runs weekly on {} at {hour:02}:{minute:02} using server local timezone {timezone_label}",
                weekday_label(weekday),
            ),
        });
    }

    let en = Regex::new(
        r"(?i)^every\s+(monday|tuesday|wednesday|thursday|friday|saturday|sunday)\s+at\s+(\d{1,2})(?::(\d{2}))?\s*(am|pm)?\s+(.+)$",
    )
    .ok()?;
    let caps = en.captures(request)?;
    let weekday = english_weekday_to_index(caps.get(1)?.as_str())?;
    let hour = caps.get(2)?.as_str().parse::<u32>().ok()?;
    let minute = caps
        .get(3)
        .and_then(|m| m.as_str().parse::<u32>().ok())
        .unwrap_or(0);
    let am_pm = caps.get(4).map(|m| m.as_str());
    let message = caps.get(5)?.as_str().trim().to_string();
    let hour = apply_am_pm(hour, am_pm)?;
    validate_hour_minute(hour, minute)?;
    let cron_weekday = cron_weekday(weekday);
    let timezone = current_local_timezone_name();
    let timezone_label = timezone.clone().unwrap_or_else(current_local_offset_label);
    Some(ParsedNaturalSchedule {
        name: derive_job_name(&message),
        message,
        schedule: CronSchedule::Cron {
            expr: format!("0 {minute} {hour} * * {cron_weekday} *"),
        },
        timezone,
        description: format!(
            "Runs weekly on {} at {hour:02}:{minute:02} using server local timezone {timezone_label}",
            weekday_label(weekday),
        ),
    })
}

/// Build a compact job name from the schedule message, keeping CJK
/// characters (so `提醒我看天气` stays readable in `/schedules` output).
fn derive_job_name(message: &str) -> String {
    let mut name = message
        .chars()
        .map(|c| if is_job_name_char(c) { c } else { '-' })
        .collect::<String>();
    while name.contains("--") {
        name = name.replace("--", "-");
    }
    let trimmed = name.trim_matches('-');
    if trimmed.is_empty() {
        "schedule".to_string()
    } else {
        truncate(trimmed, 24)
    }
}

fn is_job_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(c, '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}' | '\u{F900}'..='\u{FAFF}')
}

fn interval_to_seconds(value: i64, unit: &str) -> Option<i64> {
    if value <= 0 {
        return None;
    }
    // Checked multiplication: an oversized natural-language interval (e.g.
    // `every 9223372036854775807 days`) must be rejected, not silently wrapped.
    // In release builds an unchecked product can wrap to a negative/zero
    // `every_ms`, which CronService treats as immediately due and re-arms with
    // zero delay — a scheduler tight-loop driven by untrusted gateway input.
    let per_unit: i64 = match unit {
        "秒" | "second" | "seconds" => 1,
        "分钟" | "minute" | "minutes" => 60,
        "小时" | "hour" | "hours" => 60 * 60,
        "天" | "day" | "days" => 24 * 60 * 60,
        _ => return None,
    };
    value.checked_mul(per_unit)
}

fn interval_to_ms(value: i64, unit: &str) -> Option<i64> {
    interval_to_seconds(value, unit).and_then(|seconds| seconds.checked_mul(1000))
}

fn interval_label(value: i64, unit: &str) -> String {
    format!("{value}{unit}")
}

fn apply_time_qualifier(hour: u32, qualifier: Option<&str>) -> Option<u32> {
    let adjusted = match qualifier {
        Some("下午") | Some("晚上") if hour < 12 => hour + 12,
        Some("中午") if hour < 11 => hour + 12,
        _ => hour,
    };
    validate_hour_minute(adjusted, 0).map(|_| adjusted)
}

fn apply_am_pm(hour: u32, am_pm: Option<&str>) -> Option<u32> {
    let mut adjusted = hour;
    match am_pm.map(|value| value.to_ascii_lowercase()) {
        Some(ref suffix) if suffix == "pm" && adjusted < 12 => adjusted += 12,
        Some(ref suffix) if suffix == "am" && adjusted == 12 => adjusted = 0,
        _ => {}
    }
    validate_hour_minute(adjusted, 0).map(|_| adjusted)
}

fn validate_hour_minute(hour: u32, minute: u32) -> Option<()> {
    if hour < 24 && minute < 60 {
        Some(())
    } else {
        None
    }
}

/// Map a Monday-based weekday index (0 = Monday) to the cron crate's
/// Sunday-based numbering (0 = Sunday).
fn cron_weekday(local_weekday: u32) -> u32 {
    if local_weekday == 6 {
        0
    } else {
        local_weekday + 1
    }
}

fn chinese_weekday_to_index(value: &str) -> Option<u32> {
    Some(match value {
        "一" => 0,
        "二" => 1,
        "三" => 2,
        "四" => 3,
        "五" => 4,
        "六" => 5,
        "日" | "天" => 6,
        _ => return None,
    })
}

fn english_weekday_to_index(value: &str) -> Option<u32> {
    Some(match value.to_ascii_lowercase().as_str() {
        "monday" => 0,
        "tuesday" => 1,
        "wednesday" => 2,
        "thursday" => 3,
        "friday" => 4,
        "saturday" => 5,
        "sunday" => 6,
        _ => return None,
    })
}

fn weekday_label(index: u32) -> &'static str {
    match index {
        0 => "Monday",
        1 => "Tuesday",
        2 => "Wednesday",
        3 => "Thursday",
        4 => "Friday",
        5 => "Saturday",
        6 => "Sunday",
        _ => "Unknown",
    }
}

fn current_local_offset_label() -> String {
    let offset = Local::now().offset().local_minus_utc();
    let sign = if offset >= 0 { '+' } else { '-' };
    let abs = offset.abs();
    let hours = abs / 3600;
    let minutes = (abs % 3600) / 60;
    format!("UTC{sign}{hours:02}:{minutes:02}")
}

fn current_local_timezone_name() -> Option<String> {
    let timezone = get_timezone().ok()?;
    if timezone.trim().is_empty() {
        None
    } else {
        Some(timezone)
    }
}

fn format_schedule_for_display(schedule: &CronSchedule) -> String {
    match schedule {
        CronSchedule::At { at_ms } => {
            if let Some(local_time) = chrono::DateTime::<Utc>::from_timestamp_millis(*at_ms)
                .map(|utc| utc.with_timezone(&Local))
            {
                format!("once at {}", local_time.format("%Y-%m-%d %H:%M:%S %z"))
            } else {
                format!("once at {at_ms}")
            }
        }
        CronSchedule::Every { every_ms } => format!("every {}s", every_ms / 1000),
        CronSchedule::Cron { expr } => format!("cron: {expr}"),
    }
}

fn job_matches_context(job: &octos_bus::CronJob, channel: &str, chat_id: &str) -> bool {
    job.payload.channel.as_deref() == Some(channel)
        && job.payload.chat_id.as_deref() == Some(chat_id)
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

    #[tokio::test]
    async fn should_create_context_bound_job_when_nl_daily_schedule() {
        let dir = tempfile::tempdir().unwrap();
        let (service, _rx) = make_service(dir.path());

        let result = CronTool::add_natural_language_for_context(
            &service,
            "matrix",
            "!room:localhost",
            "每天早上 9 点提醒我看天气",
        )
        .unwrap();

        assert!(result.success, "got: {}", result.output);
        let jobs = service.list_all_jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].payload.channel.as_deref(), Some("matrix"));
        assert_eq!(jobs[0].payload.chat_id.as_deref(), Some("!room:localhost"));
        assert_eq!(jobs[0].payload.message, "提醒我看天气");
    }

    #[tokio::test]
    async fn should_create_one_shot_job_when_nl_relative_delay() {
        let dir = tempfile::tempdir().unwrap();
        let (service, _rx) = make_service(dir.path());

        let before = chrono::Utc::now().timestamp_millis();
        let result = CronTool::add_natural_language_for_context(
            &service,
            "matrix",
            "!room:localhost",
            "20秒之后提醒我看天气",
        )
        .unwrap();
        let after = chrono::Utc::now().timestamp_millis();

        assert!(result.success, "got: {}", result.output);
        assert!(result.output.contains("Runs once in 20秒"));

        let jobs = service.list_all_jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].payload.message, "提醒我看天气");
        match jobs[0].schedule {
            CronSchedule::At { at_ms } => {
                assert!(at_ms >= before + 20_000);
                assert!(at_ms <= after + 20_000);
            }
            ref other => panic!("expected one-shot schedule, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn should_create_one_shot_job_when_nl_english_relative_delay() {
        let dir = tempfile::tempdir().unwrap();
        let (service, _rx) = make_service(dir.path());

        let result = CronTool::add_natural_language_for_context(
            &service,
            "matrix",
            "!room:localhost",
            "in 10 minutes remind me to stretch",
        )
        .unwrap();

        assert!(result.success, "got: {}", result.output);
        let jobs = service.list_all_jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].payload.message, "remind me to stretch");
        assert!(matches!(jobs[0].schedule, CronSchedule::At { .. }));
    }

    #[tokio::test]
    async fn should_reject_nl_schedule_when_zero_interval() {
        let dir = tempfile::tempdir().unwrap();
        let (service, _rx) = make_service(dir.path());

        let result = CronTool::add_natural_language_for_context(
            &service,
            "matrix",
            "!room:localhost",
            "每0秒检查系统状态",
        )
        .unwrap();

        assert!(!result.success);
        assert!(result.output.contains("couldn't understand"));
        assert!(service.list_all_jobs().is_empty());
    }

    #[test]
    fn should_reject_interval_when_multiplication_overflows() {
        // `i64::MAX` days overflows `value * 24 * 60 * 60` in release builds and
        // wraps to a negative/zero interval; checked arithmetic must reject it
        // (returning None) rather than feed a tight-loop interval to CronService.
        assert_eq!(interval_to_seconds(i64::MAX, "days"), None);
        assert_eq!(interval_to_seconds(i64::MAX, "天"), None);
        assert_eq!(interval_to_seconds(i64::MAX, "hours"), None);
        assert_eq!(interval_to_ms(i64::MAX, "seconds"), None);
        // Valid intervals still compute exactly.
        assert_eq!(interval_to_seconds(5, "minutes"), Some(300));
        assert_eq!(interval_to_ms(2, "hours"), Some(2 * 60 * 60 * 1000));
    }

    #[tokio::test]
    async fn should_reject_nl_schedule_when_interval_overflows() {
        let dir = tempfile::tempdir().unwrap();
        let (service, _rx) = make_service(dir.path());

        let result = CronTool::add_natural_language_for_context(
            &service,
            "matrix",
            "!room:localhost",
            "每9223372036854775807天检查系统状态",
        )
        .unwrap();

        assert!(!result.success);
        assert!(service.list_all_jobs().is_empty());
    }

    #[tokio::test]
    async fn should_return_clarification_when_nl_schedule_ambiguous() {
        let dir = tempfile::tempdir().unwrap();
        let (service, _rx) = make_service(dir.path());

        let result = CronTool::add_natural_language_for_context(
            &service,
            "matrix",
            "!room:localhost",
            "下次提醒我看天气",
        )
        .unwrap();

        assert!(!result.success);
        assert!(result.output.contains("couldn't understand"));
        assert!(service.list_all_jobs().is_empty());
    }

    #[test]
    fn should_preserve_cjk_when_deriving_job_name() {
        assert_eq!(
            derive_job_name("检查系统状态，告诉我"),
            "检查系统状态-告诉我"
        );
        assert_eq!(derive_job_name("提醒我看天气"), "提醒我看天气");
    }

    #[test]
    fn should_preserve_local_timezone_when_parsing_daily_schedule() {
        let parsed = parse_daily_schedule("每天早上 9 点提醒我看天气").unwrap();
        assert_eq!(parsed.message, "提醒我看天气");
        match parsed.schedule {
            CronSchedule::Cron { expr } => assert_eq!(expr, "0 0 9 * * * *"),
            ref other => panic!("expected cron schedule, got {other:?}"),
        }
        assert_eq!(parsed.timezone, current_local_timezone_name());
    }

    #[test]
    fn should_preserve_local_timezone_when_parsing_weekly_schedule() {
        let parsed = parse_weekly_schedule("每周一早上 9 点提醒我看天气").unwrap();
        assert_eq!(parsed.message, "提醒我看天气");
        match parsed.schedule {
            CronSchedule::Cron { expr } => assert_eq!(expr, "0 0 9 * * 1 *"),
            ref other => panic!("expected cron schedule, got {other:?}"),
        }
        assert_eq!(parsed.timezone, current_local_timezone_name());
    }

    #[test]
    fn should_map_pm_qualifiers_when_parsing_daily_schedule() {
        let parsed = parse_daily_schedule("每天下午 3 点提醒我喝水").unwrap();
        match parsed.schedule {
            CronSchedule::Cron { expr } => assert_eq!(expr, "0 0 15 * * * *"),
            ref other => panic!("expected cron schedule, got {other:?}"),
        }

        let parsed = parse_daily_schedule("every day at 9pm review notes").unwrap();
        match parsed.schedule {
            CronSchedule::Cron { expr } => assert_eq!(expr, "0 0 21 * * * *"),
            ref other => panic!("expected cron schedule, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn should_only_show_matching_chat_when_listing_for_context() {
        let dir = tempfile::tempdir().unwrap();
        let (service, _rx) = make_service(dir.path());

        service
            .add_job(
                "room-a".into(),
                CronSchedule::Every { every_ms: 60_000 },
                CronPayload {
                    message: "A".into(),
                    deliver: true,
                    channel: Some("matrix".into()),
                    chat_id: Some("!room-a:localhost".into()),
                },
            )
            .unwrap();
        service
            .add_job(
                "room-b".into(),
                CronSchedule::Every { every_ms: 60_000 },
                CronPayload {
                    message: "B".into(),
                    deliver: true,
                    channel: Some("matrix".into()),
                    chat_id: Some("!room-b:localhost".into()),
                },
            )
            .unwrap();

        let result = CronTool::list_jobs_for_context(&service, "matrix", "!room-a:localhost");

        assert!(result.success);
        assert!(result.output.contains("room-a"));
        assert!(!result.output.contains("room-b"));
    }

    #[tokio::test]
    async fn should_reject_foreign_job_id_when_removing_for_context() {
        let dir = tempfile::tempdir().unwrap();
        let (service, _rx) = make_service(dir.path());

        let job = service
            .add_job(
                "room-b".into(),
                CronSchedule::Every { every_ms: 60_000 },
                CronPayload {
                    message: "B".into(),
                    deliver: true,
                    channel: Some("matrix".into()),
                    chat_id: Some("!room-b:localhost".into()),
                },
            )
            .unwrap();

        let result =
            CronTool::remove_job_for_context(&service, "matrix", "!room-a:localhost", &job.id);

        assert!(!result.success);
        assert!(result.output.contains("not found in this chat"));
        assert_eq!(service.list_all_jobs().len(), 1);
    }
}
