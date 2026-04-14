//! Sub-account management tools.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;

use super::{AdminApiContext, ProfileIdInput, Tool, ToolResult, format_duration};

// ── admin_list_sub_accounts ───────────────────────────────────────────

pub struct ListSubAccountsTool {
    ctx: Arc<AdminApiContext>,
}

impl ListSubAccountsTool {
    pub fn new(ctx: Arc<AdminApiContext>) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for ListSubAccountsTool {
    fn name(&self) -> &str {
        "admin_list_sub_accounts"
    }
    fn description(&self) -> &str {
        "List all sub-accounts for a given parent profile. Returns each sub-account's ID, name, status, channels, and config."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "profile_id": { "type": "string", "description": "Parent profile ID" }
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
                "/api/admin/profiles/{}/accounts",
                input.profile_id
            ))
            .await
        {
            Ok(subs) => {
                let items = subs.as_array().cloned().unwrap_or_default();
                if items.is_empty() {
                    return Ok(ToolResult {
                        output: format!(
                            "No sub-accounts found for profile '{}'.",
                            input.profile_id
                        ),
                        success: true,
                        ..Default::default()
                    });
                }

                let mut lines = Vec::new();
                for item in &items {
                    let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                    let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                    let enabled = item
                        .get("enabled")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let status = item.get("status").unwrap_or(&serde_json::Value::Null);
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

                    let channels = item
                        .get("config")
                        .and_then(|c| c.get("channels"))
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|c| c.get("type").and_then(|t| t.as_str()))
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_default();

                    let state = if running { "RUNNING" } else { "STOPPED" };
                    let en = if enabled { "enabled" } else { "disabled" };
                    lines.push(format!(
                        "- **{name}** ({id}) [{state}] {pid} {uptime} ({en}) channels=[{channels}]"
                    ));
                }

                Ok(ToolResult {
                    output: format!(
                        "{} sub-accounts for '{}':\n{}",
                        items.len(),
                        input.profile_id,
                        lines.join("\n")
                    ),
                    success: true,
                    ..Default::default()
                })
            }
            Err(e) => Ok(ToolResult {
                output: format!("Failed to list sub-accounts: {e}"),
                success: false,
                ..Default::default()
            }),
        }
    }
}

// ── admin_create_sub_account ──────────────────────────────────────────

pub struct CreateSubAccountTool {
    ctx: Arc<AdminApiContext>,
}

impl CreateSubAccountTool {
    pub fn new(ctx: Arc<AdminApiContext>) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct CreateSubAccountInput {
    profile_id: String,
    sub_account_id: String,
    name: String,
    public_subdomain: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    channels: Vec<serde_json::Value>,
    #[serde(default)]
    system_prompt: Option<String>,
    #[serde(default)]
    env_vars: std::collections::HashMap<String, String>,
}

#[async_trait]
impl Tool for CreateSubAccountTool {
    fn name(&self) -> &str {
        "admin_create_sub_account"
    }
    fn description(&self) -> &str {
        "Create a sub-account under a parent profile. The sub-account inherits LLM provider config from the parent but has its own channels, system prompt, and data directories."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "profile_id": { "type": "string", "description": "Parent profile ID" },
                "sub_account_id": { "type": "string", "description": "Immutable sub-account ID suffix (without the parent prefix)" },
                "name": { "type": "string", "description": "Name for the sub-account (e.g. 'work bot', 'support')" },
                "public_subdomain": { "type": "string", "description": "Public host slug for this sub-account" },
                "email": { "type": "string", "description": "Email address for web client OTP login (optional)" },
                "channels": {
                    "type": "array",
                    "description": "Channel configurations (e.g. [{\"Telegram\": {\"token_env\": \"WORK_TG_TOKEN\"}}])",
                    "items": { "type": "object" }
                },
                "system_prompt": { "type": "string", "description": "Custom system prompt for this sub-account" },
                "env_vars": {
                    "type": "object",
                    "description": "Environment variables specific to this sub-account",
                    "additionalProperties": { "type": "string" }
                }
            },
            "required": ["profile_id", "sub_account_id", "name", "public_subdomain"]
        })
    }
    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: CreateSubAccountInput =
            serde_json::from_value(args.clone()).map_err(|e| eyre::eyre!("invalid input: {e}"))?;

        let mut body = serde_json::json!({
            "sub_account_id": input.sub_account_id,
            "name": input.name,
            "public_subdomain": input.public_subdomain,
            "channels": input.channels,
            "env_vars": input.env_vars,
        });

        if let Some(email) = &input.email {
            body["email"] = serde_json::json!(email);
        }

        if let Some(prompt) = &input.system_prompt {
            body["gateway"] = serde_json::json!({
                "system_prompt": prompt,
            });
        }

        match self
            .ctx
            .post(
                &format!("/api/admin/profiles/{}/accounts", input.profile_id),
                Some(&body),
            )
            .await
        {
            Ok(resp) => {
                let sub_id = resp.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                let name = resp.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                Ok(ToolResult {
                    output: format!(
                        "Created sub-account '{name}' ({sub_id}) under parent '{}'.",
                        input.profile_id
                    ),
                    success: true,
                    ..Default::default()
                })
            }
            Err(e) => Ok(ToolResult {
                output: format!("Failed to create sub-account: {e}"),
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

    // -- ListSubAccountsTool --

    #[test]
    fn list_sub_accounts_metadata() {
        let tool = ListSubAccountsTool::new(ctx());
        assert_eq!(tool.name(), "admin_list_sub_accounts");
        assert!(tool.description().contains("sub-account"));
        let schema = tool.input_schema();
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(required, vec!["profile_id"]);
    }

    // -- CreateSubAccountTool --

    #[test]
    fn create_sub_account_metadata() {
        let tool = CreateSubAccountTool::new(ctx());
        assert_eq!(tool.name(), "admin_create_sub_account");
        assert!(tool.description().contains("sub-account"));
    }

    #[test]
    fn create_sub_account_schema_required_fields() {
        let tool = CreateSubAccountTool::new(ctx());
        let schema = tool.input_schema();
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(required.contains(&"profile_id"));
        assert!(required.contains(&"name"));
    }

    #[test]
    fn create_sub_account_schema_has_channels_array() {
        let tool = CreateSubAccountTool::new(ctx());
        let schema = tool.input_schema();
        assert_eq!(schema["properties"]["channels"]["type"], "array");
    }

    #[test]
    fn create_sub_account_input_minimal() {
        let v = serde_json::json!({
            "profile_id": "p1",
            "sub_account_id": "test-bot",
            "public_subdomain": "test-bot",
            "name": "test bot"
        });
        let input: CreateSubAccountInput = serde_json::from_value(v).unwrap();
        assert_eq!(input.profile_id, "p1");
        assert_eq!(input.sub_account_id, "test-bot");
        assert_eq!(input.public_subdomain, "test-bot");
        assert_eq!(input.name, "test bot");
        assert!(input.channels.is_empty());
        assert!(input.system_prompt.is_none());
        assert!(input.env_vars.is_empty());
    }

    #[test]
    fn create_sub_account_input_full() {
        let v = serde_json::json!({
            "profile_id": "p1",
            "sub_account_id": "work",
            "public_subdomain": "workbot",
            "name": "work",
            "channels": [{"Telegram": {"token_env": "TG_TOKEN"}}],
            "system_prompt": "You are helpful.",
            "env_vars": {"API_KEY": "secret"}
        });
        let input: CreateSubAccountInput = serde_json::from_value(v).unwrap();
        assert_eq!(input.sub_account_id, "work");
        assert_eq!(input.public_subdomain, "workbot");
        assert_eq!(input.channels.len(), 1);
        assert_eq!(input.system_prompt.as_deref(), Some("You are helpful."));
        assert_eq!(input.env_vars.get("API_KEY").unwrap(), "secret");
    }
}
