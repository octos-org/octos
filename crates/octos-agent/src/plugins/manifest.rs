//! Plugin manifest parsing.

use std::collections::HashMap;

use serde::Deserialize;

/// A plugin manifest (manifest.json).
#[derive(Debug, Deserialize)]
pub struct PluginManifest {
    /// Plugin name.
    pub name: String,
    /// Plugin version.
    pub version: String,
    /// Tools provided by this plugin.
    #[serde(default)]
    pub tools: Vec<PluginToolDef>,
    /// SHA-256 hash of the plugin executable for integrity verification.
    #[serde(default)]
    pub sha256: Option<String>,
    /// Pre-built binaries keyed by `{os}-{arch}` (e.g. "darwin-aarch64", "linux-x86_64").
    /// Each entry has `url` (download URL) and optional `sha256` (integrity hash).
    /// CI/CD updates this on each release.
    #[serde(default)]
    pub binaries: HashMap<String, BinaryDownload>,
    /// Whether the plugin needs network access (informational).
    #[serde(default)]
    pub requires_network: bool,
    /// Override default execution timeout in seconds.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// MCP servers this skill provides.
    #[serde(default)]
    pub mcp_servers: Vec<SkillMcpServer>,
    /// Lifecycle hooks this skill provides.
    #[serde(default)]
    pub hooks: Vec<SkillHookDef>,
    /// Prompt fragments to inject into the system prompt.
    #[serde(default)]
    pub prompts: Option<SkillPrompts>,
}

impl PluginManifest {
    /// Whether this manifest declares any extras (MCP servers, hooks, or prompts).
    pub fn has_extras(&self) -> bool {
        !self.mcp_servers.is_empty()
            || !self.hooks.is_empty()
            || self.prompts.as_ref().is_some_and(|p| !p.include.is_empty())
    }
}

/// An MCP server declared by a skill manifest.
#[derive(Debug, Clone, Deserialize)]
pub struct SkillMcpServer {
    /// Command to spawn (resolved relative to skill dir at load time).
    #[serde(default)]
    pub command: Option<String>,
    /// Arguments to the command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variable NAMES to forward from the process env.
    #[serde(default)]
    pub env: Vec<String>,
    /// HTTP transport: URL of the MCP server endpoint.
    #[serde(default)]
    pub url: Option<String>,
    /// HTTP transport: additional headers.
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

/// A lifecycle hook declared by a skill manifest.
#[derive(Debug, Clone, Deserialize)]
pub struct SkillHookDef {
    /// Lifecycle event name: "before_tool_call", "after_tool_call", etc.
    pub event: String,
    /// Command as argv array. Relative paths resolved against skill directory.
    pub command: Vec<String>,
    /// Timeout in milliseconds.
    #[serde(default = "default_hook_timeout_ms")]
    pub timeout_ms: u64,
    /// Tool name filter (empty = all tools).
    #[serde(default)]
    pub tool_filter: Vec<String>,
}

fn default_hook_timeout_ms() -> u64 {
    5000
}

/// Prompt fragment configuration.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SkillPrompts {
    /// Glob patterns for markdown files to include (relative to skill dir).
    #[serde(default)]
    pub include: Vec<String>,
}

/// A tool definition within a plugin manifest.
#[derive(Debug, Clone, Deserialize)]
pub struct PluginToolDef {
    /// Tool name (must be unique across all plugins).
    pub name: String,
    /// Description for the LLM.
    pub description: String,
    /// JSON Schema for input parameters.
    #[serde(default = "default_schema")]
    pub input_schema: serde_json::Value,
}

/// Binary download info for a specific platform.
#[derive(Debug, Clone, Deserialize)]
pub struct BinaryDownload {
    /// Download URL for the pre-built binary.
    pub url: String,
    /// SHA-256 hash for integrity verification.
    #[serde(default)]
    pub sha256: Option<String>,
}

fn default_schema() -> serde_json::Value {
    serde_json::json!({"type": "object"})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_manifest() {
        let json = r#"{
            "name": "test-plugin",
            "version": "0.1.0",
            "tools": [
                {
                    "name": "hello",
                    "description": "Say hello",
                    "input_schema": {"type": "object", "properties": {"name": {"type": "string"}}}
                }
            ]
        }"#;
        let manifest: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(manifest.name, "test-plugin");
        assert_eq!(manifest.tools.len(), 1);
        assert_eq!(manifest.tools[0].name, "hello");
    }

    #[test]
    fn test_default_schema() {
        let json = r#"{
            "name": "minimal",
            "version": "1.0.0",
            "tools": [{"name": "t", "description": "d"}]
        }"#;
        let manifest: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(
            manifest.tools[0].input_schema,
            serde_json::json!({"type": "object"})
        );
    }

    #[test]
    fn test_all_optional_fields_set() {
        let json = r#"{
            "name": "full-plugin",
            "version": "2.0.0",
            "tools": [{"name": "t", "description": "d"}],
            "sha256": "abc123def456",
            "requires_network": true,
            "timeout_secs": 30
        }"#;
        let manifest: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(manifest.name, "full-plugin");
        assert_eq!(manifest.sha256.as_deref(), Some("abc123def456"));
        assert!(manifest.requires_network);
        assert_eq!(manifest.timeout_secs, Some(30));
    }

    #[test]
    fn test_empty_tools_array() {
        let json = r#"{
            "name": "no-tools",
            "version": "1.0.0",
            "tools": []
        }"#;
        let manifest: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(manifest.name, "no-tools");
        assert!(manifest.tools.is_empty());
    }

    #[test]
    fn test_missing_name_fails() {
        let json = r#"{
            "version": "1.0.0",
            "tools": []
        }"#;
        let result = serde_json::from_str::<PluginManifest>(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_missing_version_fails() {
        let json = r#"{
            "name": "bad-plugin",
            "tools": []
        }"#;
        let result = serde_json::from_str::<PluginManifest>(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_multiple_tools() {
        let json = r#"{
            "name": "multi-tool",
            "version": "1.0.0",
            "tools": [
                {"name": "alpha", "description": "First tool"},
                {"name": "beta", "description": "Second tool"},
                {"name": "gamma", "description": "Third tool"}
            ]
        }"#;
        let manifest: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(manifest.tools.len(), 3);
        assert_eq!(manifest.tools[0].name, "alpha");
        assert_eq!(manifest.tools[1].name, "beta");
        assert_eq!(manifest.tools[2].name, "gamma");
    }

    #[test]
    fn test_complex_nested_input_schema() {
        let json = r#"{
            "name": "complex-plugin",
            "version": "1.0.0",
            "tools": [{
                "name": "deploy",
                "description": "Deploy service",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "service": {
                            "type": "object",
                            "properties": {
                                "name": {"type": "string"},
                                "replicas": {"type": "integer", "minimum": 1}
                            },
                            "required": ["name"]
                        },
                        "env_vars": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "key": {"type": "string"},
                                    "value": {"type": "string"}
                                },
                                "required": ["key", "value"]
                            }
                        },
                        "config": {
                            "oneOf": [
                                {"type": "string"},
                                {"type": "object", "additionalProperties": {"type": "string"}}
                            ]
                        }
                    },
                    "required": ["service"]
                }
            }]
        }"#;
        let manifest: PluginManifest = serde_json::from_str(json).unwrap();
        let schema = &manifest.tools[0].input_schema;
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["service"]["type"], "object");
        assert_eq!(schema["properties"]["env_vars"]["type"], "array");
        assert_eq!(
            schema["properties"]["env_vars"]["items"]["properties"]["key"]["type"],
            "string"
        );
        assert!(schema["properties"]["config"]["oneOf"].is_array());
        assert_eq!(schema["required"], serde_json::json!(["service"]));
    }
}
