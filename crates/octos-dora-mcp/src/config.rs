//! Configuration loader for dora tool mappings.

use crate::DoraToolMapping;
use serde::{Deserialize, Serialize};

/// Top-level bridge configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeConfig {
    /// Human-readable description.
    #[serde(default)]
    pub description: String,
    /// Tool mappings.
    pub mappings: Vec<DoraToolMapping>,
}

impl BridgeConfig {
    /// Load from a JSON string.
    ///
    /// # Errors
    ///
    /// Returns an error if the JSON is malformed or missing required fields.
    pub fn from_json(json: &str) -> eyre::Result<Self> {
        let config: Self = serde_json::from_str(json)?;
        Ok(config)
    }

    /// Load from a JSON file path.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or the JSON is invalid.
    pub fn from_file(path: &str) -> eyre::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        Self::from_json(&content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_agent::SafetyTier;

    #[test]
    fn should_parse_config_from_json() {
        let json = r#"{
            "description": "UR5e inspection tools",
            "mappings": [
                {
                    "tool_name": "scan_station",
                    "description": "Scan station for objects",
                    "dora_node_id": "moveit-skills",
                    "dora_output_id": "skill_request",
                    "parameters": {"station": "Station ID to scan"},
                    "safety_tier": "full_actuation",
                    "timeout_secs": 120
                }
            ]
        }"#;
        let config = BridgeConfig::from_json(json).unwrap();
        assert_eq!(config.mappings.len(), 1);
        assert_eq!(config.mappings[0].tool_name, "scan_station");
        assert_eq!(config.mappings[0].safety_tier, SafetyTier::FullActuation);
    }

    #[test]
    fn should_default_description_to_empty_string() {
        let json = r#"{"mappings": []}"#;
        let config = BridgeConfig::from_json(json).unwrap();
        assert_eq!(config.description, "");
    }

    #[test]
    fn should_fail_on_invalid_json() {
        let result = BridgeConfig::from_json("{not valid json}");
        assert!(result.is_err());
    }

    #[test]
    fn should_reject_unknown_safety_tier_string() {
        // Strict parser: typos / forward-looking tier names must fail loading
        // rather than silently downgrade to Observe (codex round-1 P1).
        let json = r#"{
            "mappings": [{
                "tool_name": "x",
                "description": "x",
                "dora_node_id": "n",
                "dora_output_id": "o",
                "parameters": {},
                "safety_tier": "kind_of_dangerous",
                "timeout_secs": 1
            }]
        }"#;
        let result = BridgeConfig::from_json(json);
        assert!(result.is_err(), "expected unknown-tier rejection");
    }

    #[test]
    fn should_default_safety_tier_when_field_omitted() {
        // Omitting `safety_tier` falls back to `Observe` (the safest default
        // for an explicitly-not-declared tool); only an explicit unknown
        // string fails. Mirrors `default_tier()` in lib.rs.
        let json = r#"{
            "mappings": [{
                "tool_name": "x",
                "description": "x",
                "dora_node_id": "n",
                "dora_output_id": "o",
                "parameters": {},
                "timeout_secs": 1
            }]
        }"#;
        let config = BridgeConfig::from_json(json).unwrap();
        assert_eq!(config.mappings[0].safety_tier, SafetyTier::Observe);
    }
}
