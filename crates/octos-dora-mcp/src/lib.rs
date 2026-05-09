//! Dora-RS to MCP tool bridge for octos.
//!
//! Wraps a [`DoraToolMapping`] (a config-defined link from a Dora node output
//! to an MCP tool name) so the agent's [`Tool`] machinery can dispatch the
//! request like any other tool. The bridge also registers the tool's
//! required safety tier in the global [`RobotToolRegistry`] so the existing
//! `group:robot:<tier>` `ToolPolicy` machinery actually applies — without
//! that registration the tier metadata would be decorative.

use std::collections::HashMap;

use async_trait::async_trait;
use octos_agent::tools::ConcurrencyClass;
use octos_agent::tools::robot_groups::{self, RobotToolRegistry};
use octos_agent::{SafetyTier, Tool, ToolResult};
use serde::{Deserialize, Serialize};

pub mod config;
pub use config::BridgeConfig;

/// Mapping from a dora-rs node output to an MCP tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoraToolMapping {
    /// MCP tool name exposed to the agent.
    pub tool_name: String,
    /// Description for the LLM.
    pub description: String,
    /// Dora node ID that handles this tool.
    pub dora_node_id: String,
    /// Dora output ID to send the request to.
    pub dora_output_id: String,
    /// Expected input parameters (name -> description).
    pub parameters: HashMap<String, String>,
    /// Required safety tier for this tool. Strict: unknown tiers fail
    /// `BridgeConfig::from_json` rather than silently defaulting to
    /// `Observe`. Serialised as snake_case (matches existing config files
    /// — `"observe"`, `"safe_motion"`, `"full_actuation"`,
    /// `"emergency_override"`).
    #[serde(default = "default_tier")]
    pub safety_tier: SafetyTier,
    /// Timeout in seconds for the tool call.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_tier() -> SafetyTier {
    SafetyTier::Observe
}

fn default_timeout() -> u64 {
    30
}

/// A bridge that wraps a [`DoraToolMapping`] as an MCP-compatible [`Tool`].
///
/// In production the `execute` method would forward the request to the dora
/// dataflow via an IPC channel and await the response.  The current
/// implementation returns a placeholder describing the would-be forwarded
/// payload so the bridge can be tested without a running dora runtime.
pub struct DoraToolBridge {
    mapping: DoraToolMapping,
}

impl DoraToolBridge {
    /// Create a new bridge from the given mapping.
    pub fn new(mapping: DoraToolMapping) -> Self {
        Self { mapping }
    }

    /// Return a reference to the underlying mapping.
    pub fn mapping(&self) -> &DoraToolMapping {
        &self.mapping
    }

    /// Required safety tier for this tool, drawn directly from the mapping.
    pub fn required_safety_tier(&self) -> SafetyTier {
        self.mapping.safety_tier
    }

    /// Build the JSON Schema object describing the tool's input parameters.
    fn build_input_schema(&self) -> serde_json::Value {
        let properties: serde_json::Map<String, serde_json::Value> = self
            .mapping
            .parameters
            .iter()
            .map(|(name, desc)| {
                (
                    name.clone(),
                    serde_json::json!({
                        "type": "string",
                        "description": desc
                    }),
                )
            })
            .collect();
        let required: Vec<String> = self.mapping.parameters.keys().cloned().collect();
        serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        })
    }
}

#[async_trait]
impl Tool for DoraToolBridge {
    fn name(&self) -> &str {
        &self.mapping.tool_name
    }

    fn description(&self) -> &str {
        &self.mapping.description
    }

    fn input_schema(&self) -> serde_json::Value {
        self.build_input_schema()
    }

    fn tags(&self) -> &[&str] {
        &["dora", "mcp-bridge", "robot"]
    }

    /// Codex round-2 P2: anything that can actuate the robot must NOT run in
    /// the same parallel batch as another bridge call against the same Dora
    /// runtime — overlapping motion commands are a real safety hazard. Only
    /// `Observe`-tier tools (sensors, status queries) keep the default
    /// parallel-friendly `Safe` class; every higher tier is `Exclusive` so
    /// the agent's batch dispatcher serialises them.
    fn concurrency_class(&self) -> ConcurrencyClass {
        match self.mapping.safety_tier {
            SafetyTier::Observe => ConcurrencyClass::Safe,
            SafetyTier::SafeMotion | SafetyTier::FullActuation | SafetyTier::EmergencyOverride => {
                ConcurrencyClass::Exclusive
            }
        }
    }

    async fn execute(&self, args: &serde_json::Value) -> eyre::Result<ToolResult> {
        let request = serde_json::json!({
            "dora_node_id": self.mapping.dora_node_id,
            "dora_output_id": self.mapping.dora_output_id,
            "tool_name": self.mapping.tool_name,
            "args": args,
            "timeout_secs": self.mapping.timeout_secs,
            "safety_tier": self.mapping.safety_tier.label(),
        });

        Ok(ToolResult {
            output: format!(
                "[dora-bridge] would forward to {}::{} with payload:\n{}",
                self.mapping.dora_node_id,
                self.mapping.dora_output_id,
                serde_json::to_string_pretty(&request).unwrap_or_default()
            ),
            success: true,
            ..Default::default()
        })
    }
}

/// Load tool mappings from a [`BridgeConfig`], create bridge tools, AND
/// register each tool with the global [`RobotToolRegistry`] at its declared
/// tier so the existing `group:robot:<tier>` `ToolPolicy` machinery sees
/// the bridge tools. Without this registration the `safety_tier` field is
/// decorative — group-based allow/deny silently misses every dora tool.
///
/// Idempotent: re-loading the same config replaces prior tier mappings for
/// the same tool name (see [`RobotToolRegistry::insert`]).
pub fn load_bridges(config: &BridgeConfig) -> Vec<DoraToolBridge> {
    let bridges: Vec<DoraToolBridge> = config
        .mappings
        .iter()
        .map(|m| DoraToolBridge::new(m.clone()))
        .collect();
    robot_groups::with_registry_mut(|reg: &mut RobotToolRegistry| {
        for bridge in &bridges {
            reg.insert(bridge.mapping.tool_name.clone(), bridge.mapping.safety_tier);
        }
    });
    bridges
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_mapping() -> DoraToolMapping {
        let mut params = HashMap::new();
        params.insert("waypoint".to_string(), "Target waypoint ID".to_string());

        DoraToolMapping {
            tool_name: "navigate_to".to_string(),
            description: "Navigate robot to a waypoint".to_string(),
            dora_node_id: "moveit-skills".to_string(),
            dora_output_id: "skill_request".to_string(),
            parameters: params,
            safety_tier: SafetyTier::SafeMotion,
            timeout_secs: 60,
        }
    }

    #[test]
    fn should_expose_correct_tool_name() {
        let bridge = DoraToolBridge::new(sample_mapping());
        assert_eq!(bridge.name(), "navigate_to");
    }

    #[test]
    fn should_expose_correct_description() {
        let bridge = DoraToolBridge::new(sample_mapping());
        assert_eq!(bridge.description(), "Navigate robot to a waypoint");
    }

    #[test]
    fn should_build_input_schema_with_parameters() {
        let bridge = DoraToolBridge::new(sample_mapping());
        let schema = bridge.input_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["waypoint"].is_object());
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "waypoint"));
    }

    #[test]
    fn should_build_empty_schema_when_no_parameters() {
        let mut mapping = sample_mapping();
        mapping.parameters.clear();
        let bridge = DoraToolBridge::new(mapping);
        let schema = bridge.input_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"].as_object().unwrap().is_empty());
    }

    #[test]
    fn should_include_dora_tags() {
        let bridge = DoraToolBridge::new(sample_mapping());
        let tags = bridge.tags();
        assert!(tags.contains(&"dora"));
        assert!(tags.contains(&"mcp-bridge"));
        assert!(tags.contains(&"robot"));
    }

    #[test]
    fn should_expose_declared_safety_tier() {
        let bridge = DoraToolBridge::new(sample_mapping());
        assert_eq!(bridge.required_safety_tier(), SafetyTier::SafeMotion);
    }

    #[test]
    fn should_fail_to_parse_unknown_safety_tier_string() {
        // Codex round-1 P1: previously an unknown / misspelled tier silently
        // collapsed to `Observe` (the LEAST restrictive), so a typo on a
        // high-risk tool would slip past observe-only allow lists. Strict
        // parsing now rejects the config.
        let json = r#"{
            "mappings": [{
                "tool_name": "rogue",
                "description": "should not load",
                "dora_node_id": "n",
                "dora_output_id": "o",
                "parameters": {},
                "safety_tier": "FullActuation",
                "timeout_secs": 1
            }]
        }"#;
        let result = BridgeConfig::from_json(json);
        assert!(result.is_err(), "expected unknown-tier rejection, got Ok");
    }

    #[test]
    fn should_register_bridges_in_robot_tool_registry_at_declared_tier() {
        // Codex round-1 P2: the bridge previously carried `safety_tier` but
        // never enrolled the tool in `RobotToolRegistry`, so the existing
        // `group:robot:<tier>` ToolPolicy machinery had nothing to evaluate
        // against. `load_bridges` now wires both halves.
        let mut high = sample_mapping();
        high.tool_name = "dora_register_high_actuation".to_string();
        high.safety_tier = SafetyTier::FullActuation;

        let mut low = sample_mapping();
        low.tool_name = "dora_register_observe".to_string();
        low.safety_tier = SafetyTier::Observe;

        let config = BridgeConfig {
            description: String::new(),
            mappings: vec![high, low],
        };
        let bridges = load_bridges(&config);
        assert_eq!(bridges.len(), 2);

        let snap = robot_groups::snapshot();
        assert_eq!(
            snap.tier_of("dora_register_high_actuation"),
            Some(SafetyTier::FullActuation),
        );
        assert_eq!(
            snap.tier_of("dora_register_observe"),
            Some(SafetyTier::Observe),
        );
    }

    #[tokio::test]
    async fn should_execute_bridge_tool_with_args() {
        let bridge = DoraToolBridge::new(sample_mapping());
        let result = bridge
            .execute(&serde_json::json!({"waypoint": "A"}))
            .await
            .unwrap();
        assert!(result.output.contains("dora-bridge"));
        assert!(result.output.contains("moveit-skills"));
        assert!(result.output.contains("safe_motion"));
        assert!(result.success);
    }
}
