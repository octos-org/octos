//! Admin tool for checking and applying octos updates via the serve API.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;

use super::{AdminApiContext, Tool, ToolResult};

pub struct UpdateCrewTool {
    ctx: Arc<AdminApiContext>,
}

impl UpdateCrewTool {
    pub fn new(ctx: Arc<AdminApiContext>) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct UpdateInput {
    action: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    github_token: Option<String>,
}

#[async_trait]
impl Tool for UpdateCrewTool {
    fn name(&self) -> &str {
        "admin_update_crew"
    }
    fn description(&self) -> &str {
        "Check for octos updates or apply an update. Actions: 'check' to see current/latest version, 'update' to download and install the latest (or a specific) version. The service restarts automatically after update. Requires 'github_token' for private repos — ask the user if not provided."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["check", "update"],
                    "description": "Action: 'check' to see versions, 'update' to apply update"
                },
                "version": {
                    "type": "string",
                    "description": "Version to update to (e.g. 'v0.2.0'). Defaults to latest."
                },
                "github_token": {
                    "type": "string",
                    "description": "GitHub personal access token for private repo access. Required for private repos."
                }
            },
            "required": ["action"]
        })
    }
    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: UpdateInput =
            serde_json::from_value(args.clone()).map_err(|e| eyre::eyre!("invalid input: {e}"))?;

        let token = input
            .github_token
            .or_else(|| std::env::var("GITHUB_TOKEN").ok());

        match input.action.as_str() {
            "check" => self.check_version(token.as_deref()).await,
            "update" => self.do_update(input.version, token.as_deref()).await,
            other => Ok(ToolResult {
                output: format!("Unknown action '{other}'. Use 'check' or 'update'."),
                success: false,
                ..Default::default()
            }),
        }
    }
}

impl UpdateCrewTool {
    async fn check_version(&self, token: Option<&str>) -> Result<ToolResult> {
        // Pass token via POST so it doesn't leak in query strings/logs
        let body = serde_json::json!({ "github_token": token });
        match self
            .ctx
            .post("/api/admin/system/version", Some(&body))
            .await
        {
            Ok(info) => {
                let current = info
                    .get("current")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let update_available = info
                    .get("update_available")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                let mut out = format!("Current version: {current}\n");

                if let Some(latest) = info.get("latest") {
                    if latest.is_null() {
                        out.push_str("Could not check latest version (GitHub API error).\n");
                    } else {
                        let tag = latest.get("tag").and_then(|v| v.as_str()).unwrap_or("?");
                        let published = latest
                            .get("published_at")
                            .and_then(|v| v.as_str())
                            .unwrap_or("?");
                        out.push_str(&format!("Latest release: {tag} (published {published})\n"));

                        if update_available {
                            out.push_str("Update available! Use action 'update' to install it.\n");
                        } else {
                            out.push_str("You are up to date.\n");
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
                output: format!("Failed to check version: {e}"),
                success: false,
                ..Default::default()
            }),
        }
    }

    async fn do_update(&self, version: Option<String>, token: Option<&str>) -> Result<ToolResult> {
        let ver = version.unwrap_or_else(|| "latest".to_string());
        let mut body = serde_json::json!({ "version": ver });
        if let Some(t) = token {
            body["github_token"] = serde_json::json!(t);
        }

        match self.ctx.post("/api/admin/system/update", Some(&body)).await {
            Ok(resp) => {
                let success = resp
                    .get("success")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let old = resp
                    .get("old_version")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let new = resp
                    .get("new_version")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let message = resp.get("message").and_then(|v| v.as_str()).unwrap_or("");
                let binaries = resp
                    .get("binaries_updated")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();

                let out = format!("Updated: {old} → {new}\nBinaries: {binaries}\n{message}");

                Ok(ToolResult {
                    output: out,
                    success,
                    ..Default::default()
                })
            }
            Err(e) => Ok(ToolResult {
                output: format!("Update failed: {e}"),
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
    fn update_crew_metadata() {
        let tool = UpdateCrewTool::new(ctx());
        assert_eq!(tool.name(), "admin_update_crew");
        assert!(tool.description().contains("update"));
    }

    #[test]
    fn update_crew_schema_action_enum() {
        let tool = UpdateCrewTool::new(ctx());
        let schema = tool.input_schema();
        let enums: Vec<&str> = schema["properties"]["action"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(enums, vec!["check", "update"]);
    }

    #[test]
    fn update_crew_schema_required_action() {
        let tool = UpdateCrewTool::new(ctx());
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
    fn update_crew_schema_has_version_field() {
        let tool = UpdateCrewTool::new(ctx());
        let schema = tool.input_schema();
        assert_eq!(schema["properties"]["version"]["type"], "string");
    }

    #[test]
    fn update_input_minimal() {
        let v = serde_json::json!({"action": "check"});
        let input: UpdateInput = serde_json::from_value(v).unwrap();
        assert_eq!(input.action, "check");
        assert!(input.version.is_none());
    }

    #[test]
    fn update_input_with_version() {
        let v = serde_json::json!({"action": "update", "version": "v0.3.0"});
        let input: UpdateInput = serde_json::from_value(v).unwrap();
        assert_eq!(input.action, "update");
        assert_eq!(input.version.as_deref(), Some("v0.3.0"));
    }

    #[test]
    fn update_input_missing_action_fails() {
        let v = serde_json::json!({});
        assert!(serde_json::from_value::<UpdateInput>(v).is_err());
    }
}
