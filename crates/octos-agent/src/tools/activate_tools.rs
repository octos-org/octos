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
        "Activate additional tool groups on demand. Call with no arguments to list \
         available tool groups, or with a group name to activate it. Activated tools \
         become available in subsequent steps."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "group": {
                    "type": "string",
                    "description": "Group name to activate (e.g. 'group:web', 'group:research'). Omit to list available groups."
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

        let group = args.get("group").and_then(|v| v.as_str()).unwrap_or("");

        if group.is_empty() {
            // List available deferred groups
            let groups = registry.deferred_groups();
            if groups.is_empty() {
                return Ok(ToolResult {
                    output: "All tools are already active. No deferred groups available."
                        .to_string(),
                    success: true,
                    ..Default::default()
                });
            }

            let mut lines = vec![
                "Available tool groups (call activate_tools with a group name to enable):"
                    .to_string(),
            ];
            for (name, description, count) in &groups {
                lines.push(format!("  {name} ({count} tools) — {description}"));
            }
            return Ok(ToolResult {
                output: lines.join("\n"),
                success: true,
                ..Default::default()
            });
        }

        // Activate the requested group
        let activated = registry.activate(group);
        if activated.is_empty() {
            Ok(ToolResult {
                output: format!(
                    "No deferred tools matched '{group}'. Call activate_tools with no arguments to see available groups."
                ),
                success: false,
                ..Default::default()
            })
        } else {
            Ok(ToolResult {
                output: format!(
                    "Activated {} tool(s): {}. They are now available for use.",
                    activated.len(),
                    activated.join(", ")
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
