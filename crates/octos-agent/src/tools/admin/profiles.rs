//! Profile management tools: list, status, start, stop, restart, enable, update.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;

use super::{AdminApiContext, ProfileIdInput, Tool, ToolResult, format_duration};

// ── admin_list_profiles ────────────────────────────────────────────────

pub struct ListProfilesTool {
    ctx: Arc<AdminApiContext>,
}

impl ListProfilesTool {
    pub fn new(ctx: Arc<AdminApiContext>) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize, Default)]
struct ListProfilesInput {
    filter: Option<String>,
}

#[async_trait]
impl Tool for ListProfilesTool {
    fn name(&self) -> &str {
        "admin_list_profiles"
    }
    fn description(&self) -> &str {
        "List all profiles with their status (running/stopped, PID, uptime). Optional filter: running, stopped, enabled, disabled."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "filter": {
                    "type": "string",
                    "enum": ["running", "stopped", "enabled", "disabled"],
                    "description": "Filter profiles by status"
                }
            }
        })
    }
    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: ListProfilesInput = serde_json::from_value(args.clone()).unwrap_or_default();

        match self.ctx.get("/api/admin/overview").await {
            Ok(overview) => {
                let profiles = overview
                    .get("profiles")
                    .and_then(|p| p.as_array())
                    .cloned()
                    .unwrap_or_default();

                let mut lines = Vec::new();
                for p in &profiles {
                    let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                    let id = p.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                    let enabled = p.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
                    let status = p.get("status").unwrap_or(&serde_json::Value::Null);
                    let running = status
                        .get("running")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let pid = status
                        .get("pid")
                        .and_then(|v| v.as_u64())
                        .map(|p| format!("PID {p}"))
                        .unwrap_or_default();
                    let uptime = status
                        .get("uptime_secs")
                        .and_then(|v| v.as_i64())
                        .map(format_duration)
                        .unwrap_or_default();

                    // Apply filter
                    match input.filter.as_deref() {
                        Some("running") if !running => continue,
                        Some("stopped") if running => continue,
                        Some("enabled") if !enabled => continue,
                        Some("disabled") if enabled => continue,
                        _ => {}
                    }

                    let state = if running { "RUNNING" } else { "STOPPED" };
                    let en = if enabled { "enabled" } else { "disabled" };
                    let provider = p
                        .get("config")
                        .and_then(|c| c.get("provider"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("-");
                    let model = p
                        .get("config")
                        .and_then(|c| c.get("model"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("-");

                    lines.push(format!(
                        "- **{name}** ({id}) [{state}] {pid} {uptime} ({en}) provider={provider} model={model}"
                    ));
                }

                if lines.is_empty() {
                    Ok(ToolResult {
                        output: "No profiles match the filter.".into(),
                        success: true,
                        ..Default::default()
                    })
                } else {
                    Ok(ToolResult {
                        output: format!("{} profiles:\n{}", lines.len(), lines.join("\n")),
                        success: true,
                        ..Default::default()
                    })
                }
            }
            Err(e) => Ok(ToolResult {
                output: format!("Failed to list profiles: {e}"),
                success: false,
                ..Default::default()
            }),
        }
    }
}

// ── admin_profile_status ───────────────────────────────────────────────

pub struct ProfileStatusTool {
    ctx: Arc<AdminApiContext>,
}

impl ProfileStatusTool {
    pub fn new(ctx: Arc<AdminApiContext>) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for ProfileStatusTool {
    fn name(&self) -> &str {
        "admin_profile_status"
    }
    fn description(&self) -> &str {
        "Get detailed status for a single profile: PID, uptime, channels, provider, model, enabled."
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
            .get(&format!("/api/admin/profiles/{}", input.profile_id))
            .await
        {
            Ok(profile) => Ok(ToolResult {
                output: serde_json::to_string_pretty(&profile).unwrap_or_else(|_| "{}".into()),
                success: true,
                ..Default::default()
            }),
            Err(e) => Ok(ToolResult {
                output: format!("Failed to get profile status: {e}"),
                success: false,
                ..Default::default()
            }),
        }
    }
}

// ── admin_start_profile ────────────────────────────────────────────────

pub struct StartProfileTool {
    ctx: Arc<AdminApiContext>,
}

impl StartProfileTool {
    pub fn new(ctx: Arc<AdminApiContext>) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for StartProfileTool {
    fn name(&self) -> &str {
        "admin_start_profile"
    }
    fn description(&self) -> &str {
        "Start a gateway for the given profile."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "profile_id": { "type": "string", "description": "Profile ID to start" }
            },
            "required": ["profile_id"]
        })
    }
    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: ProfileIdInput =
            serde_json::from_value(args.clone()).map_err(|e| eyre::eyre!("invalid input: {e}"))?;

        match self
            .ctx
            .post(
                &format!("/api/admin/profiles/{}/start", input.profile_id),
                None,
            )
            .await
        {
            Ok(resp) => {
                let msg = resp
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Started");
                Ok(ToolResult {
                    output: format!("Gateway '{}': {msg}", input.profile_id),
                    success: true,
                    ..Default::default()
                })
            }
            Err(e) => Ok(ToolResult {
                output: format!("Failed to start '{}': {e}", input.profile_id),
                success: false,
                ..Default::default()
            }),
        }
    }
}

// ── admin_stop_profile ─────────────────────────────────────────────────

pub struct StopProfileTool {
    ctx: Arc<AdminApiContext>,
}

impl StopProfileTool {
    pub fn new(ctx: Arc<AdminApiContext>) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for StopProfileTool {
    fn name(&self) -> &str {
        "admin_stop_profile"
    }
    fn description(&self) -> &str {
        "Stop a running gateway for the given profile."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "profile_id": { "type": "string", "description": "Profile ID to stop" }
            },
            "required": ["profile_id"]
        })
    }
    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: ProfileIdInput =
            serde_json::from_value(args.clone()).map_err(|e| eyre::eyre!("invalid input: {e}"))?;

        match self
            .ctx
            .post(
                &format!("/api/admin/profiles/{}/stop", input.profile_id),
                None,
            )
            .await
        {
            Ok(resp) => {
                let msg = resp
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Stopped");
                Ok(ToolResult {
                    output: format!("Gateway '{}': {msg}", input.profile_id),
                    success: true,
                    ..Default::default()
                })
            }
            Err(e) => Ok(ToolResult {
                output: format!("Failed to stop '{}': {e}", input.profile_id),
                success: false,
                ..Default::default()
            }),
        }
    }
}

// ── admin_restart_profile ──────────────────────────────────────────────

pub struct RestartProfileTool {
    ctx: Arc<AdminApiContext>,
}

impl RestartProfileTool {
    pub fn new(ctx: Arc<AdminApiContext>) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for RestartProfileTool {
    fn name(&self) -> &str {
        "admin_restart_profile"
    }
    fn description(&self) -> &str {
        "Restart a gateway (stop then start)."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "profile_id": { "type": "string", "description": "Profile ID to restart" }
            },
            "required": ["profile_id"]
        })
    }
    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: ProfileIdInput =
            serde_json::from_value(args.clone()).map_err(|e| eyre::eyre!("invalid input: {e}"))?;

        match self
            .ctx
            .post(
                &format!("/api/admin/profiles/{}/restart", input.profile_id),
                None,
            )
            .await
        {
            Ok(resp) => {
                let msg = resp
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Restarted");
                Ok(ToolResult {
                    output: format!("Gateway '{}': {msg}", input.profile_id),
                    success: true,
                    ..Default::default()
                })
            }
            Err(e) => Ok(ToolResult {
                output: format!("Failed to restart '{}': {e}", input.profile_id),
                success: false,
                ..Default::default()
            }),
        }
    }
}

// ── admin_enable_profile ───────────────────────────────────────────────

pub struct EnableProfileTool {
    ctx: Arc<AdminApiContext>,
}

impl EnableProfileTool {
    pub fn new(ctx: Arc<AdminApiContext>) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct EnableInput {
    profile_id: String,
    enabled: bool,
}

#[async_trait]
impl Tool for EnableProfileTool {
    fn name(&self) -> &str {
        "admin_enable_profile"
    }
    fn description(&self) -> &str {
        "Enable or disable auto-start for a profile."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "profile_id": { "type": "string", "description": "Profile ID" },
                "enabled": { "type": "boolean", "description": "true to enable auto-start, false to disable" }
            },
            "required": ["profile_id", "enabled"]
        })
    }
    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: EnableInput =
            serde_json::from_value(args.clone()).map_err(|e| eyre::eyre!("invalid input: {e}"))?;

        let body = serde_json::json!({ "enabled": input.enabled });
        match self
            .ctx
            .put(&format!("/api/admin/profiles/{}", input.profile_id), &body)
            .await
        {
            Ok(_) => {
                let state = if input.enabled { "enabled" } else { "disabled" };
                Ok(ToolResult {
                    output: format!("Profile '{}' auto-start {}.", input.profile_id, state),
                    success: true,
                    ..Default::default()
                })
            }
            Err(e) => Ok(ToolResult {
                output: format!("Failed to update '{}': {e}", input.profile_id),
                success: false,
                ..Default::default()
            }),
        }
    }
}

// ── admin_update_profile ──────────────────────────────────────────────

pub struct UpdateProfileTool {
    ctx: Arc<AdminApiContext>,
}

impl UpdateProfileTool {
    pub fn new(ctx: Arc<AdminApiContext>) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct UpdateProfileInput {
    profile_id: String,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    api_key_env: Option<String>,
    #[serde(default)]
    system_prompt: Option<String>,
    #[serde(default)]
    max_iterations: Option<u32>,
    #[serde(default)]
    max_history: Option<usize>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    email: Option<serde_json::Value>,
    #[serde(default)]
    env_vars: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default = "default_true")]
    restart: bool,
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Arc<AdminApiContext> {
        super::super::test_ctx()
    }

    // -- ListProfilesTool --

    #[test]
    fn list_profiles_tool_metadata() {
        let tool = ListProfilesTool::new(ctx());
        assert_eq!(tool.name(), "admin_list_profiles");
        assert!(!tool.description().is_empty());
        assert_eq!(tool.tags(), &[] as &[&str]);
    }

    #[test]
    fn list_profiles_schema_has_filter_enum() {
        let tool = ListProfilesTool::new(ctx());
        let schema = tool.input_schema();
        let filter = &schema["properties"]["filter"];
        assert_eq!(filter["type"], "string");
        let enums: Vec<&str> = filter["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(enums, vec!["running", "stopped", "enabled", "disabled"]);
    }

    #[test]
    fn list_profiles_schema_no_required() {
        let tool = ListProfilesTool::new(ctx());
        let schema = tool.input_schema();
        // filter is optional, so no "required" key
        assert!(schema.get("required").is_none());
    }

    // -- ProfileStatusTool --

    #[test]
    fn profile_status_tool_metadata() {
        let tool = ProfileStatusTool::new(ctx());
        assert_eq!(tool.name(), "admin_profile_status");
        let schema = tool.input_schema();
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(required, vec!["profile_id"]);
    }

    // -- StartProfileTool --

    #[test]
    fn start_profile_tool_metadata() {
        let tool = StartProfileTool::new(ctx());
        assert_eq!(tool.name(), "admin_start_profile");
        let schema = tool.input_schema();
        assert_eq!(schema["properties"]["profile_id"]["type"], "string");
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(required, vec!["profile_id"]);
    }

    // -- StopProfileTool --

    #[test]
    fn stop_profile_tool_metadata() {
        let tool = StopProfileTool::new(ctx());
        assert_eq!(tool.name(), "admin_stop_profile");
        let schema = tool.input_schema();
        assert_eq!(schema["properties"]["profile_id"]["type"], "string");
    }

    // -- RestartProfileTool --

    #[test]
    fn restart_profile_tool_metadata() {
        let tool = RestartProfileTool::new(ctx());
        assert_eq!(tool.name(), "admin_restart_profile");
        assert!(tool.description().contains("Restart"));
    }

    // -- EnableProfileTool --

    #[test]
    fn enable_profile_tool_schema_requires_both_fields() {
        let tool = EnableProfileTool::new(ctx());
        assert_eq!(tool.name(), "admin_enable_profile");
        let schema = tool.input_schema();
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(required.contains(&"profile_id"));
        assert!(required.contains(&"enabled"));
    }

    #[test]
    fn enable_profile_schema_enabled_is_boolean() {
        let tool = EnableProfileTool::new(ctx());
        let schema = tool.input_schema();
        assert_eq!(schema["properties"]["enabled"]["type"], "boolean");
    }

    // -- UpdateProfileTool --

    #[test]
    fn update_profile_tool_metadata() {
        let tool = UpdateProfileTool::new(ctx());
        assert_eq!(tool.name(), "admin_update_profile");
        let schema = tool.input_schema();
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(required, vec!["profile_id"]);
    }

    #[test]
    fn update_profile_schema_has_expected_properties() {
        let tool = UpdateProfileTool::new(ctx());
        let schema = tool.input_schema();
        let props = schema["properties"].as_object().unwrap();
        let expected_keys = [
            "profile_id",
            "provider",
            "model",
            "base_url",
            "api_key_env",
            "system_prompt",
            "max_iterations",
            "max_history",
            "name",
            "enabled",
            "email",
            "env_vars",
            "restart",
        ];
        for key in &expected_keys {
            assert!(props.contains_key(*key), "missing property: {key}");
        }
    }

    #[test]
    fn update_profile_restart_defaults_true() {
        // default_true is used for serde default
        assert!(default_true());
    }

    // -- EnableInput deserialization --

    #[test]
    fn enable_input_deserialize() {
        let v = serde_json::json!({"profile_id": "p1", "enabled": true});
        let input: EnableInput = serde_json::from_value(v).unwrap();
        assert_eq!(input.profile_id, "p1");
        assert!(input.enabled);
    }

    // -- ListProfilesInput deserialization --

    #[test]
    fn list_profiles_input_defaults_to_no_filter() {
        let v = serde_json::json!({});
        let input: ListProfilesInput = serde_json::from_value(v).unwrap();
        assert!(input.filter.is_none());
    }

    // -- UpdateProfileInput deserialization --

    #[test]
    fn update_profile_input_minimal() {
        let v = serde_json::json!({"profile_id": "p1"});
        let input: UpdateProfileInput = serde_json::from_value(v).unwrap();
        assert_eq!(input.profile_id, "p1");
        assert!(input.restart); // default_true
        assert!(input.provider.is_none());
        assert!(input.model.is_none());
    }
}

#[async_trait]
impl Tool for UpdateProfileTool {
    fn name(&self) -> &str {
        "admin_update_profile"
    }
    fn description(&self) -> &str {
        "Update a profile's configuration: model, provider, system prompt, etc. By default restarts the gateway to apply changes."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "profile_id": { "type": "string", "description": "Profile ID to update" },
                "provider": { "type": "string", "description": "LLM provider name (e.g. moonshot, deepseek, openai, anthropic, gemini)" },
                "model": { "type": "string", "description": "Model name (e.g. kimi-2.5, deepseek-chat, gpt-4o)" },
                "base_url": { "type": "string", "description": "Custom API base URL" },
                "api_key_env": { "type": "string", "description": "Environment variable name for API key" },
                "system_prompt": { "type": "string", "description": "Custom system prompt for the gateway" },
                "max_iterations": { "type": "integer", "description": "Max tool-call iterations per message" },
                "max_history": { "type": "integer", "description": "Max conversation history messages" },
                "name": { "type": "string", "description": "Display name for the profile" },
                "enabled": { "type": "boolean", "description": "Enable/disable auto-start" },
                "email": {
                    "type": "object",
                    "description": "Email sending config. For SMTP: {provider:'smtp', smtp_host, smtp_port, username, password, from_address}. For Feishu: {provider:'feishu', feishu_app_id, feishu_app_secret, feishu_from_address}.",
                    "properties": {
                        "provider": { "type": "string", "enum": ["smtp", "feishu"], "description": "Email provider type" },
                        "smtp_host": { "type": "string", "description": "SMTP server hostname (e.g. smtp.gmail.com)" },
                        "smtp_port": { "type": "integer", "description": "SMTP port (465 for TLS, 587 for STARTTLS)" },
                        "username": { "type": "string", "description": "SMTP login username" },
                        "password": { "type": "string", "description": "SMTP password (app password for Gmail)" },
                        "password_env": { "type": "string", "description": "Env var name holding SMTP password (legacy, prefer 'password')" },
                        "from_address": { "type": "string", "description": "Sender email address" },
                        "feishu_app_id": { "type": "string", "description": "Feishu app ID" },
                        "feishu_app_secret": { "type": "string", "description": "Feishu app secret" },
                        "feishu_from_address": { "type": "string", "description": "Feishu sender email address" }
                    }
                },
                "env_vars": {
                    "type": "object",
                    "description": "Environment variables to set (e.g. SMTP_PASSWORD, API keys). Keys are var names, values are secrets.",
                    "additionalProperties": { "type": "string" }
                },
                "restart": { "type": "boolean", "description": "Auto-restart gateway after update (default true)", "default": true }
            },
            "required": ["profile_id"]
        })
    }
    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: UpdateProfileInput =
            serde_json::from_value(args.clone()).map_err(|e| eyre::eyre!("invalid input: {e}"))?;

        // Auto-detect provider from model name when provider is not explicitly set
        let provider = input.provider.clone().or_else(|| {
            input
                .model
                .as_deref()
                .and_then(octos_llm::registry::detect_provider)
                .map(String::from)
        });

        // Build the update body with only specified fields
        let mut body = serde_json::Map::new();
        let mut config = serde_json::Map::new();
        let mut gateway = serde_json::Map::new();

        if let Some(ref name) = input.name {
            body.insert("name".into(), serde_json::json!(name));
        }
        if let Some(enabled) = input.enabled {
            body.insert("enabled".into(), serde_json::json!(enabled));
        }
        if let Some(ref provider) = provider {
            config.insert("provider".into(), serde_json::json!(provider));
        }
        if let Some(ref model) = input.model {
            config.insert("model".into(), serde_json::json!(model));
        }
        if let Some(ref base_url) = input.base_url {
            config.insert("base_url".into(), serde_json::json!(base_url));
        }
        if let Some(ref api_key_env) = input.api_key_env {
            config.insert("api_key_env".into(), serde_json::json!(api_key_env));
        }
        if let Some(ref sp) = input.system_prompt {
            gateway.insert("system_prompt".into(), serde_json::json!(sp));
        }
        if let Some(mi) = input.max_iterations {
            gateway.insert("max_iterations".into(), serde_json::json!(mi));
        }
        if let Some(mh) = input.max_history {
            gateway.insert("max_history".into(), serde_json::json!(mh));
        }
        if let Some(ref email) = input.email {
            config.insert("email".into(), email.clone());
        }
        if let Some(ref env_vars) = input.env_vars {
            config.insert(
                "env_vars".into(),
                serde_json::Value::Object(env_vars.clone()),
            );
        }
        if !gateway.is_empty() {
            config.insert("gateway".into(), serde_json::Value::Object(gateway));
        }
        if !config.is_empty() {
            body.insert("config".into(), serde_json::Value::Object(config));
        }

        if body.is_empty() {
            return Ok(ToolResult {
                output:
                    "No fields to update. Specify at least one field (provider, model, name, etc.)."
                        .into(),
                success: false,
                ..Default::default()
            });
        }

        let body_value = serde_json::Value::Object(body);
        match self
            .ctx
            .put(
                &format!("/api/admin/profiles/{}", input.profile_id),
                &body_value,
            )
            .await
        {
            Ok(_) => {
                let mut changes: Vec<String> = Vec::new();
                if let Some(ref p) = provider {
                    changes.push(format!("provider={p}"));
                }
                if let Some(ref m) = input.model {
                    changes.push(format!("model={m}"));
                }
                if let Some(ref n) = input.name {
                    changes.push(format!("name={n}"));
                }
                if let Some(enabled) = input.enabled {
                    changes.push(format!("enabled={enabled}"));
                }
                if input.system_prompt.is_some() {
                    changes.push("system_prompt=<updated>".into());
                }
                if let Some(mi) = input.max_iterations {
                    changes.push(format!("max_iterations={mi}"));
                }
                if let Some(mh) = input.max_history {
                    changes.push(format!("max_history={mh}"));
                }
                if input.base_url.is_some() {
                    changes.push("base_url=<updated>".into());
                }
                if input.api_key_env.is_some() {
                    changes.push("api_key_env=<updated>".into());
                }
                if input.email.is_some() {
                    changes.push("email=<configured>".into());
                }
                if input.env_vars.is_some() {
                    changes.push("env_vars=<updated>".into());
                }

                let mut msg = format!(
                    "Profile '{}' updated: {}",
                    input.profile_id,
                    changes.join(", ")
                );

                // Auto-restart if requested
                if input.restart {
                    match self
                        .ctx
                        .post(
                            &format!("/api/admin/profiles/{}/restart", input.profile_id),
                            None,
                        )
                        .await
                    {
                        Ok(_) => msg.push_str(". Gateway restarted."),
                        Err(e) => msg.push_str(&format!(". Restart failed: {e}")),
                    }
                }

                Ok(ToolResult {
                    output: msg,
                    success: true,
                    ..Default::default()
                })
            }
            Err(e) => Ok(ToolResult {
                output: format!("Failed to update '{}': {e}", input.profile_id),
                success: false,
                ..Default::default()
            }),
        }
    }
}
