//! System monitoring tools: health, metrics, logs, watchdog, provider metrics.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;

use super::{AdminApiContext, ProfileIdInput, Tool, ToolResult, format_duration};

// ── admin_view_logs ────────────────────────────────────────────────────

pub struct ViewLogsTool {
    ctx: Arc<AdminApiContext>,
}

impl ViewLogsTool {
    pub fn new(ctx: Arc<AdminApiContext>) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct ViewLogsInput {
    profile_id: String,
    #[serde(default = "default_log_lines")]
    lines: usize,
}
fn default_log_lines() -> usize {
    30
}

#[async_trait]
impl Tool for ViewLogsTool {
    fn name(&self) -> &str {
        "admin_view_logs"
    }
    fn description(&self) -> &str {
        "View recent log output from a running gateway. Streams SSE log events for up to 3 seconds and returns up to N lines (default 30, max 100)."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "profile_id": { "type": "string", "description": "Profile ID" },
                "lines": { "type": "integer", "description": "Number of log lines to collect (default 30, max 100)" }
            },
            "required": ["profile_id"]
        })
    }
    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: ViewLogsInput =
            serde_json::from_value(args.clone()).map_err(|e| eyre::eyre!("invalid input: {e}"))?;
        let max_lines = input.lines.min(100);

        // Connect to SSE log stream
        let url = format!(
            "{}/api/admin/profiles/{}/logs?token={}",
            self.ctx.serve_url, input.profile_id, self.ctx.admin_token
        );

        let resp = match self.ctx.http.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                return Ok(ToolResult {
                    output: format!("Failed to connect to log stream: {e}"),
                    success: false,
                    ..Default::default()
                });
            }
        };

        if !resp.status().is_success() {
            return Ok(ToolResult {
                output: format!(
                    "Gateway '{}' is not running or logs unavailable.",
                    input.profile_id
                ),
                success: false,
                ..Default::default()
            });
        }

        // Read SSE events for up to 3 seconds
        let mut lines_collected = Vec::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);

        let mut stream = resp.bytes_stream();
        use futures::StreamExt;
        let mut buffer = String::new();

        loop {
            if lines_collected.len() >= max_lines {
                break;
            }
            tokio::select! {
                chunk = stream.next() => {
                    match chunk {
                        Some(Ok(bytes)) => {
                            buffer.push_str(&String::from_utf8_lossy(&bytes));
                            while let Some(pos) = buffer.find('\n') {
                                let line = buffer[..pos].trim().to_string();
                                buffer = buffer[pos + 1..].to_string();
                                if let Some(data) = line.strip_prefix("data:") {
                                    let data = data.trim();
                                    if !data.is_empty() {
                                        lines_collected.push(data.to_string());
                                        if lines_collected.len() >= max_lines {
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                        Some(Err(_)) | None => break,
                    }
                }
                _ = tokio::time::sleep_until(deadline) => break,
            }
        }

        if lines_collected.is_empty() {
            Ok(ToolResult {
                output: format!(
                    "No log output from '{}' in the last 3 seconds.",
                    input.profile_id
                ),
                success: true,
                ..Default::default()
            })
        } else {
            Ok(ToolResult {
                output: format!(
                    "{} log lines from '{}':\n{}",
                    lines_collected.len(),
                    input.profile_id,
                    lines_collected.join("\n")
                ),
                success: true,
                ..Default::default()
            })
        }
    }
}

// ── admin_system_health ────────────────────────────────────────────────

pub struct SystemHealthTool {
    ctx: Arc<AdminApiContext>,
}

impl SystemHealthTool {
    pub fn new(ctx: Arc<AdminApiContext>) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for SystemHealthTool {
    fn name(&self) -> &str {
        "admin_system_health"
    }
    fn description(&self) -> &str {
        "Get system-wide health: total profiles, running/stopped counts, server uptime."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    async fn execute(&self, _args: &serde_json::Value) -> Result<ToolResult> {
        match self.ctx.get("/api/admin/overview").await {
            Ok(overview) => {
                let total = overview
                    .get("total_profiles")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let running = overview
                    .get("running")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let stopped = overview
                    .get("stopped")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);

                let mut out = String::from("System Health:\n");
                out.push_str(&format!("  Profiles: {total} total\n"));
                out.push_str(&format!("  Running: {running}, Stopped: {stopped}\n"));

                if let Some(profiles) = overview.get("profiles").and_then(|p| p.as_array()) {
                    let down: Vec<_> = profiles
                        .iter()
                        .filter(|p| {
                            let enabled =
                                p.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
                            let running = p
                                .get("status")
                                .and_then(|s| s.get("running"))
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            enabled && !running
                        })
                        .collect();
                    if !down.is_empty() {
                        out.push_str("\n  WARNING: Some enabled profiles are not running!\n");
                        for p in &down {
                            let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                            let id = p.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                            out.push_str(&format!("    - {name} ({id})\n"));
                        }
                    }
                }

                Ok(ToolResult {
                    output: out,
                    success: true,
                    ..Default::default()
                })
            }
            Err(e) => Ok(ToolResult {
                output: format!("Failed to get system health: {e}"),
                success: false,
                ..Default::default()
            }),
        }
    }
}

// ── admin_provider_metrics ─────────────────────────────────────────────

pub struct ProviderMetricsTool {
    ctx: Arc<AdminApiContext>,
}

impl ProviderMetricsTool {
    pub fn new(ctx: Arc<AdminApiContext>) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for ProviderMetricsTool {
    fn name(&self) -> &str {
        "admin_provider_metrics"
    }
    fn description(&self) -> &str {
        "Read provider QoS metrics (latency, error rate, token usage) for a profile."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "profile_id": { "type": "string", "description": "Profile ID" }
            },
            "required": ["profile_id"]
        })
    }
    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: ProfileIdInput =
            serde_json::from_value(args.clone()).map_err(|e| eyre::eyre!("invalid input: {e}"))?;

        match self
            .ctx
            .get(&format!("/api/admin/profiles/{}/metrics", input.profile_id))
            .await
        {
            Ok(metrics) => Ok(ToolResult {
                output: serde_json::to_string_pretty(&metrics).unwrap_or_else(|_| "{}".into()),
                success: true,
                ..Default::default()
            }),
            Err(e) => Ok(ToolResult {
                output: format!("No metrics available for '{}': {e}", input.profile_id),
                success: true,
                ..Default::default()
            }),
        }
    }
}

// ── admin_manage_watchdog ──────────────────────────────────────────────

pub struct ManageWatchdogTool {
    ctx: Arc<AdminApiContext>,
}

impl ManageWatchdogTool {
    pub fn new(ctx: Arc<AdminApiContext>) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct WatchdogInput {
    action: String,
}

#[async_trait]
impl Tool for ManageWatchdogTool {
    fn name(&self) -> &str {
        "admin_manage_watchdog"
    }
    fn description(&self) -> &str {
        "Check or toggle watchdog auto-restart and proactive alerts. Actions: status, enable, disable."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["status", "enable", "disable"],
                    "description": "Action to perform"
                }
            },
            "required": ["action"]
        })
    }
    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: WatchdogInput =
            serde_json::from_value(args.clone()).map_err(|e| eyre::eyre!("invalid input: {e}"))?;

        let path = match input.action.as_str() {
            "status" => "/api/admin/monitor/status",
            "enable" | "disable" => {
                let endpoint = "/api/admin/monitor/watchdog";
                let body = serde_json::json!({ "enabled": input.action == "enable" });
                match self.ctx.post(endpoint, Some(&body)).await {
                    Ok(resp) => {
                        let msg = resp
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("Done");
                        return Ok(ToolResult {
                            output: msg.to_string(),
                            success: true,
                            ..Default::default()
                        });
                    }
                    Err(e) => {
                        return Ok(ToolResult {
                            output: format!("Failed: {e}"),
                            success: false,
                            ..Default::default()
                        });
                    }
                }
            }
            other => {
                return Ok(ToolResult {
                    output: format!("Unknown action '{other}'. Use: status, enable, disable."),
                    success: false,
                    ..Default::default()
                });
            }
        };

        match self.ctx.get(path).await {
            Ok(status) => Ok(ToolResult {
                output: serde_json::to_string_pretty(&status).unwrap_or_else(|_| "{}".into()),
                success: true,
                ..Default::default()
            }),
            Err(e) => Ok(ToolResult {
                output: format!("Failed: {e}"),
                success: false,
                ..Default::default()
            }),
        }
    }
}

// ── admin_system_metrics ──────────────────────────────────────────────

pub struct SystemMetricsTool {
    ctx: Arc<AdminApiContext>,
}

impl SystemMetricsTool {
    pub fn new(ctx: Arc<AdminApiContext>) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for SystemMetricsTool {
    fn name(&self) -> &str {
        "admin_system_metrics"
    }
    fn description(&self) -> &str {
        "Get system resource metrics: CPU usage, memory, swap, disk storage, and platform info."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    async fn execute(&self, _args: &serde_json::Value) -> Result<ToolResult> {
        match self.ctx.get("/api/admin/system/metrics").await {
            Ok(data) => {
                let mut out = String::new();

                if let Some(cpu) = data.get("cpu") {
                    let usage = cpu
                        .get("usage_percent")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0);
                    let cores = cpu.get("core_count").and_then(|v| v.as_u64()).unwrap_or(0);
                    let brand = cpu
                        .get("brand")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    out.push_str(&format!("CPU: {brand}\n"));
                    out.push_str(&format!("  Usage: {usage:.1}% ({cores} cores)\n"));
                }

                if let Some(mem) = data.get("memory") {
                    let total = mem.get("total_bytes").and_then(|v| v.as_u64()).unwrap_or(0);
                    let used = mem.get("used_bytes").and_then(|v| v.as_u64()).unwrap_or(0);
                    let avail = mem
                        .get("available_bytes")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let pct = if total > 0 {
                        (used as f64 / total as f64) * 100.0
                    } else {
                        0.0
                    };
                    out.push_str(&format!(
                        "\nMemory: {:.1} GB used / {:.1} GB total ({pct:.0}%)\n",
                        used as f64 / 1_073_741_824.0,
                        total as f64 / 1_073_741_824.0,
                    ));
                    out.push_str(&format!(
                        "  Available: {:.1} GB\n",
                        avail as f64 / 1_073_741_824.0,
                    ));
                }

                if let Some(swap) = data.get("swap") {
                    let total = swap
                        .get("total_bytes")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let used = swap.get("used_bytes").and_then(|v| v.as_u64()).unwrap_or(0);
                    if total > 0 {
                        out.push_str(&format!(
                            "\nSwap: {:.1} GB used / {:.1} GB total\n",
                            used as f64 / 1_073_741_824.0,
                            total as f64 / 1_073_741_824.0,
                        ));
                    }
                }

                if let Some(disks) = data.get("disks").and_then(|v| v.as_array()) {
                    out.push_str("\nStorage:\n");
                    for d in disks {
                        let mount = d.get("mount_point").and_then(|v| v.as_str()).unwrap_or("?");
                        let total = d.get("total_bytes").and_then(|v| v.as_u64()).unwrap_or(0);
                        let used = d.get("used_bytes").and_then(|v| v.as_u64()).unwrap_or(0);
                        let avail = d
                            .get("available_bytes")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        let pct = if total > 0 {
                            (used as f64 / total as f64) * 100.0
                        } else {
                            0.0
                        };
                        out.push_str(&format!(
                            "  {mount}: {:.1} GB used / {:.1} GB total ({pct:.0}%), {:.1} GB free\n",
                            used as f64 / 1_073_741_824.0,
                            total as f64 / 1_073_741_824.0,
                            avail as f64 / 1_073_741_824.0,
                        ));
                    }
                }

                if let Some(plat) = data.get("platform") {
                    let host = plat.get("hostname").and_then(|v| v.as_str()).unwrap_or("?");
                    let os = plat.get("os").and_then(|v| v.as_str()).unwrap_or("?");
                    let ver = plat
                        .get("os_version")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");
                    let uptime = plat
                        .get("uptime_secs")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    out.push_str(&format!("\nPlatform: {os} {ver} ({host})\n"));
                    out.push_str(&format!("  Uptime: {}\n", format_duration(uptime)));
                }

                Ok(ToolResult {
                    output: out,
                    success: true,
                    ..Default::default()
                })
            }
            Err(e) => Ok(ToolResult {
                output: format!("Failed to get system metrics: {e}"),
                success: false,
                ..Default::default()
            }),
        }
    }
}

// ── admin_view_sessions ──────────────────────────────────────────────

pub struct ViewSessionsTool {
    ctx: Arc<AdminApiContext>,
}

impl ViewSessionsTool {
    pub fn new(ctx: Arc<AdminApiContext>) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct ViewSessionsInput {
    profile_id: String,
    /// If provided, read this specific session's messages. Otherwise list all sessions.
    #[serde(default)]
    session_key: Option<String>,
    /// Max messages to return when reading a session (default 30, max 100).
    #[serde(default = "default_session_lines")]
    lines: usize,
}

fn default_session_lines() -> usize {
    30
}

#[async_trait]
impl Tool for ViewSessionsTool {
    fn name(&self) -> &str {
        "admin_view_sessions"
    }
    fn description(&self) -> &str {
        "List or read session history for a profile. Without session_key, lists all sessions. With session_key, returns recent messages from that session. Use this to diagnose conversation issues, check cron job results (system:cron_* sessions), or review tool call outcomes."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "profile_id": { "type": "string", "description": "Profile ID" },
                "session_key": { "type": "string", "description": "Session key to read (e.g. 'system:cron_abc123' or 'telegram:12345'). Omit to list all sessions." },
                "lines": { "type": "integer", "description": "Max messages to return when reading a session (default 30, max 100)" }
            },
            "required": ["profile_id"]
        })
    }
    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: ViewSessionsInput =
            serde_json::from_value(args.clone()).map_err(|e| eyre::eyre!("invalid input: {e}"))?;

        if let Some(ref key) = input.session_key {
            // Read specific session
            let max_lines = input.lines.min(100);
            // Simple percent-encode for query parameter
            let encoded_key: String = key
                .bytes()
                .map(|b| {
                    if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'~'
                    {
                        format!("{}", b as char)
                    } else {
                        format!("%{b:02X}")
                    }
                })
                .collect();
            let url = format!(
                "/api/admin/profiles/{}/sessions/read?key={}&lines={}",
                input.profile_id, encoded_key, max_lines
            );
            match self.ctx.get(&url).await {
                Ok(data) => {
                    let total = data
                        .get("total_messages")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let returned = data.get("returned").and_then(|v| v.as_u64()).unwrap_or(0);
                    let updated = data
                        .get("updated_at")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");

                    let mut out = format!(
                        "Session '{}' ({} total messages, showing last {}, updated: {}):\n\n",
                        key, total, returned, updated
                    );

                    if let Some(messages) = data.get("messages").and_then(|v| v.as_array()) {
                        for msg in messages {
                            let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("?");
                            let content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
                            out.push_str(&format!("[{}] {}\n", role, content));
                            if let Some(tcs) = msg.get("tool_calls").and_then(|v| v.as_array()) {
                                for tc in tcs {
                                    let name =
                                        tc.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                                    let args =
                                        tc.get("arguments").and_then(|v| v.as_str()).unwrap_or("");
                                    out.push_str(&format!("  -> tool_call: {}({})\n", name, args));
                                }
                            }
                        }
                    }

                    Ok(ToolResult {
                        output: out,
                        success: true,
                        ..Default::default()
                    })
                }
                Err(e) => Ok(ToolResult {
                    output: format!("Failed to read session: {e}"),
                    success: false,
                    ..Default::default()
                }),
            }
        } else {
            // List all sessions
            match self
                .ctx
                .get(&format!(
                    "/api/admin/profiles/{}/sessions",
                    input.profile_id
                ))
                .await
            {
                Ok(data) => {
                    let count = data.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
                    let mut out = format!("{} sessions for '{}':\n\n", count, input.profile_id);

                    if let Some(sessions) = data.get("sessions").and_then(|v| v.as_array()) {
                        for s in sessions {
                            let key = s.get("key").and_then(|v| v.as_str()).unwrap_or("?");
                            let msgs = s.get("messages").and_then(|v| v.as_u64()).unwrap_or(0);
                            let modified =
                                s.get("modified").and_then(|v| v.as_str()).unwrap_or("?");
                            let size = s.get("size_bytes").and_then(|v| v.as_u64()).unwrap_or(0);
                            out.push_str(&format!(
                                "  {} — {} msgs, {}KB, modified: {}\n",
                                key,
                                msgs,
                                size / 1024,
                                modified
                            ));
                        }
                    }

                    Ok(ToolResult {
                        output: out,
                        success: true,
                        ..Default::default()
                    })
                }
                Err(e) => Ok(ToolResult {
                    output: format!("Failed to list sessions: {e}"),
                    success: false,
                    ..Default::default()
                }),
            }
        }
    }
}

// ── admin_cron_status ───────────────────────────────────────────────

pub struct CronStatusTool {
    ctx: Arc<AdminApiContext>,
}

impl CronStatusTool {
    pub fn new(ctx: Arc<AdminApiContext>) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for CronStatusTool {
    fn name(&self) -> &str {
        "admin_cron_status"
    }
    fn description(&self) -> &str {
        "List cron jobs for a profile with schedule, last run time, last status, and next fire time. Use to diagnose missed or failed cron jobs."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "profile_id": { "type": "string", "description": "Profile ID" }
            },
            "required": ["profile_id"]
        })
    }
    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: ProfileIdInput =
            serde_json::from_value(args.clone()).map_err(|e| eyre::eyre!("invalid input: {e}"))?;

        match self
            .ctx
            .get(&format!("/api/admin/profiles/{}/cron", input.profile_id))
            .await
        {
            Ok(data) => {
                let count = data.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
                if count == 0 {
                    return Ok(ToolResult {
                        output: format!("No cron jobs configured for '{}'.", input.profile_id),
                        success: true,
                        ..Default::default()
                    });
                }

                let mut out = format!("{} cron jobs for '{}':\n\n", count, input.profile_id);

                if let Some(jobs) = data.get("jobs").and_then(|v| v.as_array()) {
                    for j in jobs {
                        let id = j.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                        let name = j.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                        let enabled = j.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
                        let message = j.get("message").and_then(|v| v.as_str()).unwrap_or("?");
                        let last_run = j
                            .get("last_run")
                            .and_then(|v| v.as_str())
                            .unwrap_or("never");
                        let last_status = j
                            .get("last_status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("n/a");
                        let next_in = j.get("next_in").and_then(|v| v.as_str()).unwrap_or("n/a");
                        let status_icon = if enabled { "ON" } else { "OFF" };

                        out.push_str(&format!(
                            "  [{status_icon}] {name} (id: {id})\n    Message: {message}\n    Last run: {last_run} — Status: {last_status}\n    Next fire in: {next_in}\n\n"
                        ));
                    }
                }

                Ok(ToolResult {
                    output: out,
                    success: true,
                    ..Default::default()
                })
            }
            Err(e) => Ok(ToolResult {
                output: format!("Failed to get cron status: {e}"),
                success: false,
                ..Default::default()
            }),
        }
    }
}

// ── admin_check_config ──────────────────────────────────────────────

pub struct CheckConfigTool {
    ctx: Arc<AdminApiContext>,
}

impl CheckConfigTool {
    pub fn new(ctx: Arc<AdminApiContext>) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for CheckConfigTool {
    fn name(&self) -> &str {
        "admin_check_config"
    }
    fn description(&self) -> &str {
        "Check a profile's runtime configuration: LLM provider, channels, email setup, env vars (names only), installed skills, and gateway status. Use to diagnose configuration issues."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "profile_id": { "type": "string", "description": "Profile ID" }
            },
            "required": ["profile_id"]
        })
    }
    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: ProfileIdInput =
            serde_json::from_value(args.clone()).map_err(|e| eyre::eyre!("invalid input: {e}"))?;

        match self
            .ctx
            .get(&format!(
                "/api/admin/profiles/{}/config-check",
                input.profile_id
            ))
            .await
        {
            Ok(data) => {
                let name = data.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                let enabled = data
                    .get("enabled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let provider = data.get("provider").and_then(|v| v.as_str()).unwrap_or("?");
                let model = data.get("model").and_then(|v| v.as_str()).unwrap_or("?");

                let mut out = format!("Config check for '{}' ({}):\n\n", input.profile_id, name);
                out.push_str(&format!("  Enabled: {enabled}\n"));
                out.push_str(&format!("  Provider: {provider}, Model: {model}\n"));

                // Gateway status
                if let Some(gw) = data.get("gateway_status") {
                    let running = gw.get("running").and_then(|v| v.as_bool()).unwrap_or(false);
                    let uptime = gw.get("uptime_secs").and_then(|v| v.as_i64());
                    if running {
                        let uptime_str = uptime
                            .map(super::format_duration)
                            .unwrap_or_else(|| "?".into());
                        out.push_str(&format!("  Gateway: RUNNING (uptime: {uptime_str})\n"));
                    } else {
                        out.push_str("  Gateway: STOPPED\n");
                    }
                }

                // Channels
                if let Some(channels) = data.get("channels").and_then(|v| v.as_array()) {
                    let ch_list: Vec<&str> = channels.iter().filter_map(|v| v.as_str()).collect();
                    out.push_str(&format!(
                        "  Channels: {}\n",
                        if ch_list.is_empty() {
                            "none".into()
                        } else {
                            ch_list.join(", ")
                        }
                    ));
                }

                // Email
                if let Some(email) = data.get("email") {
                    let configured = email
                        .get("configured")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    if configured {
                        out.push_str("  Email (SMTP): CONFIGURED\n");
                    } else {
                        let mut missing = Vec::new();
                        if !email
                            .get("smtp_host")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                        {
                            missing.push("host");
                        }
                        if !email
                            .get("username")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                        {
                            missing.push("username");
                        }
                        if !email
                            .get("password")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                        {
                            missing.push("password");
                        }
                        out.push_str(&format!(
                            "  Email (SMTP): NOT CONFIGURED (missing: {})\n",
                            missing.join(", ")
                        ));
                    }
                }

                // Env vars
                if let Some(vars) = data.get("env_vars").and_then(|v| v.as_array()) {
                    let var_names: Vec<&str> = vars.iter().filter_map(|v| v.as_str()).collect();
                    out.push_str(&format!(
                        "  Env vars: {} set ({})\n",
                        var_names.len(),
                        if var_names.is_empty() {
                            "none".into()
                        } else {
                            var_names.join(", ")
                        }
                    ));
                }

                // Skills
                if let Some(skills) = data.get("installed_skills").and_then(|v| v.as_array()) {
                    let skill_names: Vec<&str> = skills.iter().filter_map(|v| v.as_str()).collect();
                    out.push_str(&format!(
                        "  Skills: {}\n",
                        if skill_names.is_empty() {
                            "none".into()
                        } else {
                            skill_names.join(", ")
                        }
                    ));
                }

                // Sessions & cron
                let sessions = data
                    .get("sessions_count")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let has_cron = data
                    .get("has_cron_jobs")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                out.push_str(&format!("  Sessions: {sessions} files\n"));
                out.push_str(&format!(
                    "  Cron jobs: {}\n",
                    if has_cron { "yes" } else { "none" }
                ));

                Ok(ToolResult {
                    output: out,
                    success: true,
                    ..Default::default()
                })
            }
            Err(e) => Ok(ToolResult {
                output: format!("Failed to check config: {e}"),
                success: false,
                ..Default::default()
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Arc<AdminApiContext> {
        super::super::test_ctx()
    }

    // -- ViewLogsTool --

    #[test]
    fn view_logs_metadata() {
        let tool = ViewLogsTool::new(ctx());
        assert_eq!(tool.name(), "admin_view_logs");
        assert!(tool.description().contains("log"));
        let schema = tool.input_schema();
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(required, vec!["profile_id"]);
        assert_eq!(schema["properties"]["lines"]["type"], "integer");
    }

    #[test]
    fn view_logs_input_defaults() {
        let v = serde_json::json!({"profile_id": "p1"});
        let input: ViewLogsInput = serde_json::from_value(v).unwrap();
        assert_eq!(input.profile_id, "p1");
        assert_eq!(input.lines, 30); // default_log_lines
    }

    #[test]
    fn default_log_lines_is_30() {
        assert_eq!(default_log_lines(), 30);
    }

    // -- SystemHealthTool --

    #[test]
    fn system_health_metadata() {
        let tool = SystemHealthTool::new(ctx());
        assert_eq!(tool.name(), "admin_system_health");
        assert!(tool.description().contains("health"));
        let schema = tool.input_schema();
        // No required fields
        assert!(schema.get("required").is_none());
        assert!(schema["properties"].as_object().unwrap().is_empty());
    }

    // -- SystemMetricsTool --

    #[test]
    fn system_metrics_metadata() {
        let tool = SystemMetricsTool::new(ctx());
        assert_eq!(tool.name(), "admin_system_metrics");
        assert!(tool.description().contains("CPU"));
        let schema = tool.input_schema();
        assert!(schema["properties"].as_object().unwrap().is_empty());
    }

    // -- ProviderMetricsTool --

    #[test]
    fn provider_metrics_metadata() {
        let tool = ProviderMetricsTool::new(ctx());
        assert_eq!(tool.name(), "admin_provider_metrics");
        let schema = tool.input_schema();
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(required, vec!["profile_id"]);
    }

    // -- ManageWatchdogTool --

    #[test]
    fn manage_watchdog_metadata() {
        let tool = ManageWatchdogTool::new(ctx());
        assert_eq!(tool.name(), "admin_manage_watchdog");
        let schema = tool.input_schema();
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(required, vec!["action"]);
        let enums: Vec<&str> = schema["properties"]["action"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(enums, vec!["status", "enable", "disable"]);
    }

    #[test]
    fn watchdog_input_deserialize() {
        let v = serde_json::json!({"action": "status"});
        let input: WatchdogInput = serde_json::from_value(v).unwrap();
        assert_eq!(input.action, "status");
    }

    #[test]
    fn watchdog_input_missing_action_fails() {
        let v = serde_json::json!({});
        assert!(serde_json::from_value::<WatchdogInput>(v).is_err());
    }
}
