//! Meta-tool for two-tier tool dispatch: activates deferred tool groups on demand.

use std::sync::Weak;

use async_trait::async_trait;
use eyre::Result;

use super::{Tool, ToolRegistry, ToolResult};

/// A meta-tool that lets the LLM discover and activate deferred tool groups.
///
/// On first call (or with no arguments), lists available groups.
/// When called with a group name, activates those tools for subsequent iterations.
pub struct ActivateToolsTool {
    registry: std::sync::Mutex<Option<Weak<ToolRegistry>>>,
}

impl Default for ActivateToolsTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ActivateToolsTool {
    pub fn new() -> Self {
        Self {
            registry: std::sync::Mutex::new(None),
        }
    }

    /// Set (or replace) the registry back-reference after Arc wrapping.
    pub fn set_registry(&self, weak: Weak<ToolRegistry>) {
        *self.registry.lock().unwrap_or_else(|e| e.into_inner()) = Some(weak);
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
                    "description": "Tool names to activate (e.g. [\"fm_tts\", \"voice_synthesize\"]). Call with no args to list available deferred tools."
                },
                "group": {
                    "type": "string",
                    "description": "Alternatively, a group name to activate all tools in it (e.g. 'group:memory')."
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
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
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
            // List available deferred tools (flat list, not groups).
            //
            // Codex round-2 BLOCK (PR #865): filter through
            // `is_tool_visible_post_activation` so we don't advertise
            // names that are policy-denied or context-hidden — calling
            // `activate()` on them would remove from `deferred` but
            // leave them invisible.
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
                        if registry.is_tool_visible_post_activation(t) {
                            tools.push((*t).to_string());
                        }
                    }
                } else if registry.is_tool_visible_post_activation(name) {
                    // Group name was the individual deferred tool name
                    // itself (plugin tool not in any policy group).
                    tools.push(name.clone());
                }
            }
            if tools.is_empty() {
                return Ok(ToolResult {
                    output: "All tools are already active.".to_string(),
                    success: true,
                    ..Default::default()
                });
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

        let mut activated_now = Vec::new();
        let mut already_active = Vec::new();

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
                    if activated.is_empty() {
                        if registry.get(tool_name).is_some() {
                            already_active.push(tool_name.clone());
                        }
                    } else {
                        activated_now.extend(activated);
                    }
                } else {
                    // Try as a direct group name
                    let activated = registry.activate(tool_name);
                    if activated.is_empty() {
                        if registry.get(tool_name).is_some() {
                            already_active.push(tool_name.clone());
                        }
                    } else {
                        activated_now.extend(activated);
                    }
                }
            }
        }

        // Legacy: activate by group name
        if !group.is_empty() {
            let activated = registry.activate(group);
            if activated.is_empty() {
                if let Some(info) = super::policy::tool_group_info(group) {
                    already_active.extend(
                        info.tools
                            .iter()
                            .filter(|&&tool| registry.get(tool).is_some())
                            .map(|&tool| tool.to_string()),
                    );
                } else if registry.get(group).is_some() {
                    already_active.push(group.to_string());
                }
            } else {
                activated_now.extend(activated);
            }
        }

        // Deduplicate
        activated_now.sort();
        activated_now.dedup();
        already_active.sort();
        already_active.dedup();

        // Codex round-2 BLOCK (PR #865): filter the output lists through
        // post-activation visibility. After `activate()` removes a name
        // from `deferred`, the tool may still be invisible because of
        // `provider_policy` or `context_filter`. We must not advertise
        // such names as "Loaded …" — the LLM would try to call them and
        // get back the same "tool not available" the BLOCK was supposed
        // to fix.
        activated_now.retain(|n| registry.is_tool_visible_post_activation(n));
        already_active.retain(|n| registry.is_tool_visible_post_activation(n));

        if activated_now.is_empty() && already_active.is_empty() {
            Ok(ToolResult {
                output: "No tools matched. Call activate_tools with no arguments to see available tools.".to_string(),
                success: false,
                ..Default::default()
            })
        } else {
            let output = match (activated_now.is_empty(), already_active.is_empty()) {
                (false, true) => format!(
                    "Loaded {} tool(s): {}",
                    activated_now.len(),
                    activated_now.join(", ")
                ),
                (true, false) => format!("Already active: {}", already_active.join(", ")),
                (false, false) => format!(
                    "Loaded {} tool(s): {}. Already active: {}",
                    activated_now.len(),
                    activated_now.join(", "),
                    already_active.join(", ")
                ),
                (true, true) => unreachable!(),
            };
            Ok(ToolResult {
                output,
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

    /// Fix #3c (2026-05-10): `specs()` enriches the `activate_tools`
    /// description with the list of currently deferred tools so the LLM
    /// has explicit discovery info. Before this fix, after the LRU
    /// evicted (or the loader deferred) tools, the LLM saw a static
    /// description with no hint that those tools could be reloaded —
    /// it would report "I don't have <tool> available" and shell-fish
    /// for workarounds.
    #[tokio::test]
    async fn activate_tools_spec_description_surfaces_deferred_names() {
        let mut registry = ToolRegistry::with_builtins(PathBuf::from("/tmp"));
        // ActivateToolsTool is wired separately by the agent setup, not by
        // with_builtins — register it explicitly here so the spec is
        // visible in the test.
        registry.register(ActivateToolsTool::new());
        registry.defer(["web_search".to_string(), "web_fetch".to_string()]);
        let registry = Arc::new(registry);

        let specs = registry.specs();
        let activate_spec = specs
            .iter()
            .find(|s| s.name == "activate_tools")
            .expect("activate_tools registered above");

        assert!(
            activate_spec.description.contains("Load additional tools"),
            "base description should remain; got: {}",
            activate_spec.description
        );
        assert!(
            activate_spec.description.contains("web_search"),
            "deferred tool web_search should appear in description; got: {}",
            activate_spec.description
        );
        assert!(
            activate_spec.description.contains("web_fetch"),
            "deferred tool web_fetch should appear in description; got: {}",
            activate_spec.description
        );
    }

    /// Codex round-1 (PR #865) BLOCK regression: when a tool is BOTH
    /// deferred AND policy-denied, the activate_tools suffix must NOT
    /// list it — calling `activate_tools` on a policy-denied name only
    /// removes it from `deferred`, leaving it still invisible because
    /// of the policy filter. Advertising it as "available to load"
    /// would mislead the LLM.
    #[tokio::test]
    async fn activate_tools_spec_description_filters_policy_denied_deferred_names() {
        use crate::tools::policy::ToolPolicy;
        let mut registry = ToolRegistry::with_builtins(PathBuf::from("/tmp"));
        registry.register(ActivateToolsTool::new());
        registry.defer(["web_search".to_string(), "web_fetch".to_string()]);
        // Deny `web_search` via provider policy. Even after activation
        // it would stay invisible — so it should NOT appear in the
        // activate_tools suffix.
        let mut policy = ToolPolicy::default();
        policy.deny.push("web_search".to_string());
        registry.set_provider_policy(policy);
        let registry = Arc::new(registry);

        let specs = registry.specs();
        let activate_spec = specs
            .iter()
            .find(|s| s.name == "activate_tools")
            .expect("activate_tools registered above");

        // web_fetch is deferred but still policy-allowed → must appear.
        assert!(
            activate_spec.description.contains("web_fetch"),
            "policy-allowed deferred tool should be listed; got: {}",
            activate_spec.description
        );
        // web_search is deferred AND policy-denied → must NOT appear.
        assert!(
            !activate_spec.description.contains("web_search"),
            "policy-denied deferred tool must NOT be listed in activate_tools suffix; got: {}",
            activate_spec.description
        );
    }

    /// Codex round-2 (PR #865) BLOCK regression: when the LLM calls
    /// `activate_tools(["web_fetch"])`, the code maps web_fetch to the
    /// group:web group and activates the whole group. The returned
    /// `activated_now` list previously included `web_search` (also in
    /// group:web) even when `web_search` was denied by provider_policy.
    /// Output formatting then printed both names, advertising a
    /// policy-denied tool as "Loaded".
    ///
    /// Also covers the no-arg listing path: when no args are passed,
    /// `activate_tools` lists deferred-group members. Policy-denied
    /// members must be filtered out of that list too.
    #[tokio::test]
    async fn activate_tools_execute_output_filters_policy_denied_group_members() {
        use crate::tools::policy::ToolPolicy;
        let mut registry = ToolRegistry::with_builtins(PathBuf::from("/tmp"));
        // Defer both web tools (simulates LRU eviction of the web group).
        registry.defer_group("group:web");
        // Deny web_search via provider policy; web_fetch stays allowed.
        let mut policy = ToolPolicy::default();
        policy.deny.push("web_search".to_string());
        registry.set_provider_policy(policy);
        let registry = Arc::new(registry);

        let tool = ActivateToolsTool::new();
        tool.set_registry(Arc::downgrade(&registry));

        // (1) No-arg listing must not advertise web_search.
        let list = tool.execute(&serde_json::json!({})).await.unwrap();
        assert!(list.success);
        assert!(
            list.output.contains("web_fetch"),
            "policy-allowed deferred tool web_fetch must be listed; got: {}",
            list.output
        );
        assert!(
            !list.output.contains("web_search"),
            "policy-denied deferred tool web_search must NOT be listed; got: {}",
            list.output
        );

        // (2) Activating web_fetch (which maps to group:web) must not
        // advertise web_search as "Loaded" even though
        // `registry.activate("group:web")` returns both names.
        let loaded = tool
            .execute(&serde_json::json!({"tools": ["web_fetch"]}))
            .await
            .unwrap();
        assert!(loaded.success);
        assert!(
            loaded.output.contains("web_fetch"),
            "web_fetch should be advertised as loaded; got: {}",
            loaded.output
        );
        assert!(
            !loaded.output.contains("web_search"),
            "policy-denied web_search must NOT appear in execute output; got: {}",
            loaded.output
        );
    }

    #[tokio::test]
    async fn activate_tools_spec_description_unchanged_when_nothing_deferred() {
        let mut registry = ToolRegistry::with_builtins(PathBuf::from("/tmp"));
        registry.register(ActivateToolsTool::new());
        let registry = Arc::new(registry);
        let specs = registry.specs();
        let activate_spec = specs
            .iter()
            .find(|s| s.name == "activate_tools")
            .expect("activate_tools registered above");
        assert!(
            !activate_spec.description.contains("Currently deferred"),
            "no enrichment suffix when nothing deferred; got: {}",
            activate_spec.description
        );
    }

    #[tokio::test]
    async fn should_list_deferred_groups_when_no_args() {
        let mut registry = ToolRegistry::with_builtins(PathBuf::from("/tmp"));
        registry.defer_group("group:web");
        let registry = Arc::new(registry);

        let tool = ActivateToolsTool::new();
        tool.set_registry(Arc::downgrade(&registry));

        let result = tool.execute(&serde_json::json!({})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("web_search"));
        assert!(result.output.contains("web_fetch"));
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

    #[tokio::test]
    async fn should_report_tool_already_active() {
        let registry = Arc::new(ToolRegistry::with_builtins(PathBuf::from("/tmp")));

        let tool = ActivateToolsTool::new();
        tool.set_registry(Arc::downgrade(&registry));

        let result = tool
            .execute(&serde_json::json!({"tools": ["shell"]}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("Already active"));
        assert!(result.output.contains("shell"));
    }

    #[tokio::test]
    async fn should_report_group_already_active() {
        let registry = Arc::new(ToolRegistry::with_builtins(PathBuf::from("/tmp")));

        let tool = ActivateToolsTool::new();
        tool.set_registry(Arc::downgrade(&registry));

        let result = tool
            .execute(&serde_json::json!({"group": "group:web"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("Already active"));
        assert!(result.output.contains("web_search"));
        assert!(result.output.contains("browser"));
    }
}
