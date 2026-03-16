//! Platform skills: server-level OminiX ASR/TTS engine management via ominix-api.
//!
//! Actions: status, health, start, stop, restart, logs, models, download_model,
//!          install, remove.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;

use super::{AdminApiContext, Tool, ToolResult};

pub struct PlatformSkillsTool {
    ctx: Arc<AdminApiContext>,
}

impl PlatformSkillsTool {
    pub fn new(ctx: Arc<AdminApiContext>) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct PlatformSkillsInput {
    action: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    model_id: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    lines: Option<usize>,
}

#[async_trait]
impl Tool for PlatformSkillsTool {
    fn name(&self) -> &str {
        "admin_platform_skills"
    }
    fn description(&self) -> &str {
        "Manage OminiX platform services — on-device ASR (speech-to-text) and TTS (text-to-speech) engines. Shared across all profiles.\n\
         Actions:\n\
         - status: List OminiX skills with installation, model, and backend health info\n\
         - health: Detailed backend health check for a service (name required)\n\
         - start: Start the OminiX engine service via launchd\n\
         - stop: Stop the OminiX engine service\n\
         - restart: Restart the OminiX engine service\n\
         - logs: View recent OminiX engine log output (optional: lines, default 50)\n\
         - models: List platform-enabled models with download status\n\
         - available_models: List ALL ominix-api models (to see what can be enabled)\n\
         - enable_model: Add a model to octos platform allowlist (model_id + role required)\n\
         - disable_model: Remove a model from octos platform allowlist (model_id required)\n\
         - download_model: Download a model by model_id (e.g. 'qwen3-asr-1.7b', 'qwen3-tts')\n\
         - remove_model: Remove a downloaded model by model_id\n\
         - install: Bootstrap an OminiX skill binary\n\
         - remove: Uninstall an OminiX skill"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["status", "health", "start", "stop", "restart", "logs",
                             "models", "available_models", "enable_model", "disable_model",
                             "download_model", "remove_model",
                             "install", "remove"],
                    "description": "Action to perform on OminiX platform skills (ASR/TTS engine)"
                },
                "name": {
                    "type": "string",
                    "description": "Service name (e.g. 'ominix-api', 'asr'). Required for health/install/remove."
                },
                "model_id": {
                    "type": "string",
                    "description": "Model identifier for download_model/remove_model/enable_model/disable_model (e.g. 'qwen3-asr-1.7b', 'qwen3-tts')"
                },
                "role": {
                    "type": "string",
                    "description": "Model role for enable_model (e.g. 'asr', 'tts')"
                },
                "lines": {
                    "type": "integer",
                    "description": "Number of log lines to return (for 'logs' action, default 50, max 200)"
                }
            },
            "required": ["action"]
        })
    }
    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: PlatformSkillsInput =
            serde_json::from_value(args.clone()).map_err(|e| eyre::eyre!("invalid input: {e}"))?;

        match input.action.as_str() {
            "status" => match self.ctx.get("/api/admin/platform-skills").await {
                Ok(resp) => Ok(ToolResult {
                    output: serde_json::to_string_pretty(&resp).unwrap_or_default(),
                    success: true,
                    ..Default::default()
                }),
                Err(e) => Ok(ToolResult {
                    output: format!("Failed to get platform skills status: {e}"),
                    success: false,
                    ..Default::default()
                }),
            },
            "health" => {
                let name = input
                    .name
                    .as_deref()
                    .ok_or_else(|| eyre::eyre!("'name' is required for health"))?;
                let path = format!("/api/admin/platform-skills/{name}/health");
                match self.ctx.get(&path).await {
                    Ok(resp) => Ok(ToolResult {
                        output: serde_json::to_string_pretty(&resp).unwrap_or_default(),
                        success: true,
                        ..Default::default()
                    }),
                    Err(e) => Ok(ToolResult {
                        output: format!("Health check failed: {e}"),
                        success: false,
                        ..Default::default()
                    }),
                }
            }
            "start" | "stop" | "restart" => {
                let path = format!("/api/admin/platform-skills/ominix-api/{}", input.action);
                match self.ctx.post(&path, None).await {
                    Ok(resp) => {
                        let msg = resp
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("Done");
                        Ok(ToolResult {
                            output: msg.to_string(),
                            success: true,
                            ..Default::default()
                        })
                    }
                    Err(e) => Ok(ToolResult {
                        output: format!("Failed to {} ominix-api: {e}", input.action),
                        success: false,
                        ..Default::default()
                    }),
                }
            }
            "logs" => {
                let lines = input.lines.unwrap_or(50).min(200);
                let path = format!("/api/admin/platform-skills/ominix-api/logs?lines={lines}");
                match self.ctx.get(&path).await {
                    Ok(resp) => Ok(ToolResult {
                        output: serde_json::to_string_pretty(&resp).unwrap_or_default(),
                        success: true,
                        ..Default::default()
                    }),
                    Err(e) => Ok(ToolResult {
                        output: format!("Failed to get logs: {e}"),
                        success: false,
                        ..Default::default()
                    }),
                }
            }
            "install" => {
                let name = input
                    .name
                    .as_deref()
                    .ok_or_else(|| eyre::eyre!("'name' is required for install"))?;
                let path = format!("/api/admin/platform-skills/{name}/install");
                match self.ctx.post(&path, None).await {
                    Ok(resp) => Ok(ToolResult {
                        output: serde_json::to_string_pretty(&resp).unwrap_or_default(),
                        success: true,
                        ..Default::default()
                    }),
                    Err(e) => Ok(ToolResult {
                        output: format!("Install failed: {e}"),
                        success: false,
                        ..Default::default()
                    }),
                }
            }
            "remove" => {
                let name = input
                    .name
                    .as_deref()
                    .ok_or_else(|| eyre::eyre!("'name' is required for remove"))?;
                let path = format!("/api/admin/platform-skills/{name}");
                match self.ctx.delete(&path).await {
                    Ok(resp) => Ok(ToolResult {
                        output: serde_json::to_string_pretty(&resp).unwrap_or_default(),
                        success: true,
                        ..Default::default()
                    }),
                    Err(e) => Ok(ToolResult {
                        output: format!("Remove failed: {e}"),
                        success: false,
                        ..Default::default()
                    }),
                }
            }
            "models" => {
                match self
                    .ctx
                    .get("/api/admin/platform-skills/ominix-api/models")
                    .await
                {
                    Ok(resp) => Ok(ToolResult {
                        output: serde_json::to_string_pretty(&resp).unwrap_or_default(),
                        success: true,
                        ..Default::default()
                    }),
                    Err(e) => Ok(ToolResult {
                        output: format!("Failed to get model catalog: {e}"),
                        success: false,
                        ..Default::default()
                    }),
                }
            }
            "download_model" => {
                let model_id = input
                    .model_id
                    .as_deref()
                    .ok_or_else(|| eyre::eyre!("'model_id' is required for download_model"))?;
                let body = serde_json::json!({ "model_id": model_id });
                match self
                    .ctx
                    .post(
                        "/api/admin/platform-skills/ominix-api/models/download",
                        Some(&body),
                    )
                    .await
                {
                    Ok(resp) => Ok(ToolResult {
                        output: serde_json::to_string_pretty(&resp).unwrap_or_default(),
                        success: true,
                        ..Default::default()
                    }),
                    Err(e) => Ok(ToolResult {
                        output: format!("Download failed: {e}"),
                        success: false,
                        ..Default::default()
                    }),
                }
            }
            "remove_model" => {
                let model_id = input
                    .model_id
                    .as_deref()
                    .ok_or_else(|| eyre::eyre!("'model_id' is required for remove_model"))?;
                let body = serde_json::json!({ "model_id": model_id });
                match self
                    .ctx
                    .post(
                        "/api/admin/platform-skills/ominix-api/models/remove",
                        Some(&body),
                    )
                    .await
                {
                    Ok(resp) => Ok(ToolResult {
                        output: serde_json::to_string_pretty(&resp).unwrap_or_default(),
                        success: true,
                        ..Default::default()
                    }),
                    Err(e) => Ok(ToolResult {
                        output: format!("Remove model failed: {e}"),
                        success: false,
                        ..Default::default()
                    }),
                }
            }
            "available_models" => {
                match self
                    .ctx
                    .get("/api/admin/platform-skills/ominix-api/models/available")
                    .await
                {
                    Ok(resp) => Ok(ToolResult {
                        output: serde_json::to_string_pretty(&resp).unwrap_or_default(),
                        success: true,
                        ..Default::default()
                    }),
                    Err(e) => Ok(ToolResult {
                        output: format!("Failed to get available models: {e}"),
                        success: false,
                        ..Default::default()
                    }),
                }
            }
            "enable_model" => {
                let model_id = input
                    .model_id
                    .as_deref()
                    .ok_or_else(|| eyre::eyre!("'model_id' is required for enable_model"))?;
                let role = input.role.as_deref().ok_or_else(|| {
                    eyre::eyre!("'role' is required for enable_model (e.g. 'asr', 'tts')")
                })?;
                let body = serde_json::json!({ "model_id": model_id, "role": role });
                match self
                    .ctx
                    .post(
                        "/api/admin/platform-skills/ominix-api/models/enable",
                        Some(&body),
                    )
                    .await
                {
                    Ok(resp) => Ok(ToolResult {
                        output: serde_json::to_string_pretty(&resp).unwrap_or_default(),
                        success: true,
                        ..Default::default()
                    }),
                    Err(e) => Ok(ToolResult {
                        output: format!("Enable model failed: {e}"),
                        success: false,
                        ..Default::default()
                    }),
                }
            }
            "disable_model" => {
                let model_id = input
                    .model_id
                    .as_deref()
                    .ok_or_else(|| eyre::eyre!("'model_id' is required for disable_model"))?;
                let body = serde_json::json!({ "model_id": model_id });
                match self
                    .ctx
                    .post(
                        "/api/admin/platform-skills/ominix-api/models/disable",
                        Some(&body),
                    )
                    .await
                {
                    Ok(resp) => Ok(ToolResult {
                        output: serde_json::to_string_pretty(&resp).unwrap_or_default(),
                        success: true,
                        ..Default::default()
                    }),
                    Err(e) => Ok(ToolResult {
                        output: format!("Disable model failed: {e}"),
                        success: false,
                        ..Default::default()
                    }),
                }
            }
            other => Ok(ToolResult {
                output: format!(
                    "Unknown action: {other}. Use: status, health, start, stop, restart, logs, models, available_models, enable_model, disable_model, download_model, remove_model, install, remove."
                ),
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

    #[test]
    fn platform_skills_metadata() {
        let tool = PlatformSkillsTool::new(ctx());
        assert_eq!(tool.name(), "admin_platform_skills");
        assert!(tool.description().contains("OminiX"));
    }

    #[test]
    fn platform_skills_schema_action_enum() {
        let tool = PlatformSkillsTool::new(ctx());
        let schema = tool.input_schema();
        let enums: Vec<&str> = schema["properties"]["action"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(
            enums,
            vec![
                "status",
                "health",
                "start",
                "stop",
                "restart",
                "logs",
                "models",
                "available_models",
                "enable_model",
                "disable_model",
                "download_model",
                "remove_model",
                "install",
                "remove"
            ]
        );
    }

    #[test]
    fn platform_skills_schema_required_action_only() {
        let tool = PlatformSkillsTool::new(ctx());
        let schema = tool.input_schema();
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(required, vec!["action"]);
    }

    #[test]
    fn platform_skills_schema_has_optional_fields() {
        let tool = PlatformSkillsTool::new(ctx());
        let schema = tool.input_schema();
        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("name"));
        assert!(props.contains_key("model_id"));
        assert!(props.contains_key("role"));
        assert!(props.contains_key("lines"));
        assert_eq!(props["lines"]["type"], "integer");
    }

    #[test]
    fn platform_skills_input_minimal() {
        let v = serde_json::json!({"action": "status"});
        let input: PlatformSkillsInput = serde_json::from_value(v).unwrap();
        assert_eq!(input.action, "status");
        assert!(input.name.is_none());
        assert!(input.model_id.is_none());
        assert!(input.lines.is_none());
    }

    #[test]
    fn platform_skills_input_full() {
        let v = serde_json::json!({
            "action": "download_model",
            "model_id": "qwen3-asr-1.7b",
            "name": "ominix-api",
            "lines": 100
        });
        let input: PlatformSkillsInput = serde_json::from_value(v).unwrap();
        assert_eq!(input.action, "download_model");
        assert_eq!(input.model_id.as_deref(), Some("qwen3-asr-1.7b"));
        assert_eq!(input.name.as_deref(), Some("ominix-api"));
        assert_eq!(input.lines, Some(100));
    }
}
