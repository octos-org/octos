//! Meta-tool for two-tier tool dispatch: activates deferred tool groups on demand.

use std::sync::{OnceLock, Weak};

use async_trait::async_trait;
use eyre::Result;

use super::{Tool, ToolRegistry, ToolResult};

/// A meta-tool that lets the LLM discover and activate deferred tool groups.
///
/// On first call (or with no arguments), lists available groups.
/// When called with a group name, activates those tools for subsequent iterations.
pub struct ActivateToolsTool {
    registry: OnceLock<Weak<ToolRegistry>>,
}

impl Default for ActivateToolsTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ActivateToolsTool {
    pub fn new() -> Self {
        Self {
            registry: OnceLock::new(),
        }
    }

    /// Set the registry back-reference after Arc wrapping.
    pub fn set_registry(&self, weak: Weak<ToolRegistry>) {
        let _ = self.registry.set(weak);
    }
}

#[async_trait]
impl Tool for ActivateToolsTool {
    fn name(&self) -> &str {
        "activate_tools"
    }

    fn description(&self) -> &str {
        "Load additional tools. Pass one or more tool names to activate them. \
         Load all tools you expect to need in a single call to save round-trips."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "tools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Tool names to activate (e.g. [\"save_memory\", \"recall_memory\"]). Omit to list available tools."
                },
                "group": {
                    "type": "string",
                    "description": "Alternatively, a group name to activate all tools in it (e.g. 'group:memory'). Prefer using 'tools' array for precise control."
                }
            }
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let registry = self
            .registry
            .get()
            .and_then(|w| w.upgrade())
            .ok_or_else(|| eyre::eyre!("tool registry not available"))?;

        // Accept either "tools" array or legacy "group" string
        let tool_names: Vec<String> = args
            .get("tools")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let group = args.get("group").and_then(|v| v.as_str()).unwrap_or("");

        if tool_names.is_empty() && group.is_empty() {
            // List available deferred tools (flat list, not groups)
            let groups = registry.deferred_groups();
            if groups.is_empty() {
                return Ok(ToolResult {
                    output: "All tools are already active.".to_string(),
                    success: true,
                    ..Default::default()
                });
            }

            let mut tools: Vec<String> = Vec::new();
            for (name, _desc, _count) in &groups {
                if let Some(info) = super::policy::TOOL_GROUPS.iter().find(|g| g.name == *name) {
                    for t in info.tools {
                        tools.push((*t).to_string());
                    }
                }
            }
            return Ok(ToolResult {
                output: format!(
                    "Available tools to load: {}. \
                     Call activate_tools with [\"tool1\", \"tool2\"] to load them.",
                    tools.join(", ")
                ),
                success: true,
                ..Default::default()
            });
        }

        let mut all_activated = Vec::new();

        // Activate by individual tool names — find which group each belongs to
        if !tool_names.is_empty() {
            for tool_name in &tool_names {
                // Find the group containing this tool
                let group_name = super::policy::TOOL_GROUPS
                    .iter()
                    .find(|g| g.tools.contains(&tool_name.as_str()))
                    .map(|g| g.name);

                if let Some(gn) = group_name {
                    let activated = registry.activate(gn);
                    all_activated.extend(activated);
                } else {
                    // Try as a direct group name
                    let activated = registry.activate(tool_name);
                    all_activated.extend(activated);
                }
            }
        }

        // Legacy: activate by group name
        if !group.is_empty() {
            let activated = registry.activate(group);
            all_activated.extend(activated);
        }

        // Deduplicate
        all_activated.sort();
        all_activated.dedup();

        if all_activated.is_empty() {
            Ok(ToolResult {
                output: "No tools matched. Call activate_tools with no arguments to see available tools.".to_string(),
                success: false,
                ..Default::default()
            })
        } else {
            Ok(ToolResult {
                output: format!(
                    "Loaded {} tool(s): {}",
                    all_activated.len(),
                    all_activated.join(", ")
                ),
                success: true,
                ..Default::default()
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolRegistry;
    use std::path::PathBuf;
    use std::sync::Arc;

    #[tokio::test]
    async fn should_list_deferred_groups_when_no_args() {
        let mut registry = ToolRegistry::with_builtins(PathBuf::from("/tmp"));
        registry.defer_group("group:web");
        let registry = Arc::new(registry);

        let tool = ActivateToolsTool::new();
        tool.set_registry(Arc::downgrade(&registry));

        let result = tool.execute(&serde_json::json!({})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("group:web"));
        assert!(result.output.contains("Web search"));
    }

    #[tokio::test]
    async fn should_activate_group_and_return_names() {
        let mut registry = ToolRegistry::with_builtins(PathBuf::from("/tmp"));
        registry.defer_group("group:web");
        assert!(!registry.specs().iter().any(|s| s.name == "web_search"));

        let registry = Arc::new(registry);

        let tool = ActivateToolsTool::new();
        tool.set_registry(Arc::downgrade(&registry));

        let result = tool
            .execute(&serde_json::json!({"group": "group:web"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("web_search"));

        // After activation, specs should include web tools
        assert!(registry.specs().iter().any(|s| s.name == "web_search"));
    }

    #[tokio::test]
    async fn should_report_no_deferred_when_all_active() {
        let registry = Arc::new(ToolRegistry::with_builtins(PathBuf::from("/tmp")));

        let tool = ActivateToolsTool::new();
        tool.set_registry(Arc::downgrade(&registry));

        let result = tool.execute(&serde_json::json!({})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("already active"));
    }

    #[tokio::test]
    async fn should_fail_on_unknown_group() {
        let mut registry = ToolRegistry::with_builtins(PathBuf::from("/tmp"));
        registry.defer_group("group:web");
        let registry = Arc::new(registry);

        let tool = ActivateToolsTool::new();
        tool.set_registry(Arc::downgrade(&registry));

        let result = tool
            .execute(&serde_json::json!({"group": "group:nonexistent"}))
            .await
            .unwrap();
        assert!(!result.success);
    }
}
