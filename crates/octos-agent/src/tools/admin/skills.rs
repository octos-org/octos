//! Per-profile skill management (install/remove GitHub skills for a profile).

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;

use super::{AdminApiContext, Tool, ToolResult};

pub struct ManageSkillsTool {
    ctx: Arc<AdminApiContext>,
}

impl ManageSkillsTool {
    pub fn new(ctx: Arc<AdminApiContext>) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct ManageSkillsInput {
    action: String,
    profile_id: String,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    force: bool,
    #[serde(default)]
    branch: Option<String>,
}

#[async_trait]
impl Tool for ManageSkillsTool {
    fn name(&self) -> &str {
        "admin_manage_skills"
    }
    fn description(&self) -> &str {
        "Manage customer-installed skills for exactly one profile or sub-account: list installed skills, install from GitHub (user/repo), or remove by name."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "install", "remove"],
                    "description": "Action to perform"
                },
                "profile_id": {
                    "type": "string",
                    "description": "Profile ID to manage skills for"
                },
                "repo": {
                    "type": "string",
                    "description": "GitHub path user/repo (required for install)"
                },
                "name": {
                    "type": "string",
                    "description": "Skill name (required for remove)"
                },
                "force": {
                    "type": "boolean",
                    "description": "Overwrite existing skills (for install)"
                },
                "branch": {
                    "type": "string",
                    "description": "Git branch (default: main)"
                }
            },
            "required": ["action", "profile_id"]
        })
    }
    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: ManageSkillsInput =
            serde_json::from_value(args.clone()).map_err(|e| eyre::eyre!("invalid input: {e}"))?;

        match input.action.as_str() {
            "list" => {
                let path = format!("/api/admin/profiles/{}/skills", input.profile_id);
                match self.ctx.get(&path).await {
                    Ok(resp) => Ok(ToolResult {
                        output: serde_json::to_string_pretty(&resp).unwrap_or_default(),
                        success: true,
                        ..Default::default()
                    }),
                    Err(e) => Ok(ToolResult {
                        output: format!("Failed to list skills: {e}"),
                        success: false,
                        ..Default::default()
                    }),
                }
            }
            "install" => {
                let repo = input
                    .repo
                    .as_deref()
                    .ok_or_else(|| eyre::eyre!("repo is required for install"))?;
                let body = serde_json::json!({
                    "repo": repo,
                    "force": input.force,
                    "branch": input.branch.as_deref().unwrap_or("main"),
                });
                let path = format!("/api/admin/profiles/{}/skills", input.profile_id);
                match self.ctx.post(&path, Some(&body)).await {
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
                    .ok_or_else(|| eyre::eyre!("name is required for remove"))?;
                let path = format!("/api/admin/profiles/{}/skills/{}", input.profile_id, name);
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
            other => Ok(ToolResult {
                output: format!("Unknown action: {other}. Use list, install, or remove."),
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
    fn manage_skills_metadata() {
        let tool = ManageSkillsTool::new(ctx());
        assert_eq!(tool.name(), "admin_manage_skills");
        assert!(tool.description().contains("skill"));
    }

    #[test]
    fn manage_skills_schema_required_fields() {
        let tool = ManageSkillsTool::new(ctx());
        let schema = tool.input_schema();
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(required.contains(&"action"));
        assert!(required.contains(&"profile_id"));
    }

    #[test]
    fn manage_skills_schema_action_enum() {
        let tool = ManageSkillsTool::new(ctx());
        let schema = tool.input_schema();
        let enums: Vec<&str> = schema["properties"]["action"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(enums, vec!["list", "install", "remove"]);
    }

    #[test]
    fn manage_skills_input_deserialize_minimal() {
        let v = serde_json::json!({"action": "list", "profile_id": "p1"});
        let input: ManageSkillsInput = serde_json::from_value(v).unwrap();
        assert_eq!(input.action, "list");
        assert_eq!(input.profile_id, "p1");
        assert!(input.repo.is_none());
        assert!(input.name.is_none());
        assert!(!input.force);
        assert!(input.branch.is_none());
    }

    #[test]
    fn manage_skills_input_deserialize_full() {
        let v = serde_json::json!({
            "action": "install",
            "profile_id": "p1",
            "repo": "user/repo",
            "force": true,
            "branch": "dev"
        });
        let input: ManageSkillsInput = serde_json::from_value(v).unwrap();
        assert_eq!(input.action, "install");
        assert_eq!(input.repo.as_deref(), Some("user/repo"));
        assert!(input.force);
        assert_eq!(input.branch.as_deref(), Some("dev"));
    }
}
