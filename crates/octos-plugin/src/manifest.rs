use std::path::Path;

use eyre::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// The type of plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginType {
    Tool,
    Skill,
    Channel,
    Hook,
}

/// A tool provided by a tool-type plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// Tool name (snake_case, globally unique).
    pub name: String,
    /// Human-readable description shown to the LLM.
    pub description: String,
    /// JSON Schema describing the tool's input.
    #[serde(default)]
    pub input_schema: serde_json::Value,
    /// Optional entrypoint override (legacy field).
    #[serde(default)]
    pub entrypoint: Option<String>,
}

/// Requirements that must be satisfied for the plugin to load.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Requirements {
    /// Binary names that must exist on PATH.
    #[serde(default)]
    pub bins: Vec<String>,
    /// Environment variable names that must be set.
    #[serde(default)]
    pub env: Vec<String>,
    /// Allowed OS values (e.g. "darwin", "linux"). Empty = any OS.
    #[serde(default)]
    pub os: Vec<String>,
}

/// An install step that can provide missing binaries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallSpec {
    /// Step identifier.
    #[serde(default)]
    pub id: Option<String>,
    /// Install method.
    pub kind: String,
    /// Homebrew formula name.
    #[serde(default)]
    pub formula: Option<String>,
    /// APT package name.
    #[serde(default)]
    pub package: Option<String>,
    /// Cargo crate name.
    #[serde(rename = "crate", default)]
    pub crate_name: Option<String>,
    /// Download URL.
    #[serde(default)]
    pub url: Option<String>,
    /// Binary names provided by this install step.
    #[serde(default)]
    pub bins: Vec<String>,
    /// Human-readable label.
    #[serde(default)]
    pub label: Option<String>,
    /// OS constraints for this install step.
    #[serde(default)]
    pub os: Vec<String>,
}

/// The parsed contents of a plugin's `manifest.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    /// Plugin identifier (kebab-case). Falls back to `name` for legacy manifests.
    #[serde(alias = "name")]
    pub id: String,

    /// Semver version string.
    pub version: String,

    /// Plugin type. Optional for backward compat with legacy manifests that
    /// have no `type` field — defaults to [`PluginType::Tool`] if `tools` is
    /// present, otherwise [`PluginType::Skill`].
    #[serde(rename = "type", default)]
    pub plugin_type: Option<PluginType>,

    /// Human-readable description.
    #[serde(default)]
    pub description: Option<String>,

    /// Author name.
    #[serde(default)]
    pub author: Option<String>,

    /// Homepage URL.
    #[serde(default)]
    pub homepage: Option<String>,

    /// License identifier (e.g. "MIT", "Apache-2.0").
    #[serde(default)]
    pub license: Option<String>,

    /// Executable binary filename (relative to plugin dir). Default: "main".
    #[serde(default)]
    pub binary: Option<String>,

    /// Tool call timeout in seconds (tool-type only).
    #[serde(default)]
    pub timeout_secs: Option<u64>,

    /// Tools provided by a tool-type plugin.
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,

    /// Hook event names for hook-type plugins.
    #[serde(default)]
    pub hooks: Vec<String>,

    /// Requirements for gating.
    #[serde(default, alias = "requirements")]
    pub requires: Option<Requirements>,

    /// JSON Schema for plugin-specific config.
    #[serde(default)]
    pub config_schema: Option<serde_json::Value>,

    /// Install steps.
    #[serde(default)]
    pub install: Vec<InstallSpec>,

    /// Legacy field: whether the plugin requires network access.
    #[serde(default)]
    pub requires_network: Option<bool>,
}

impl PluginManifest {
    /// Parse a manifest from a JSON string.
    pub fn from_json(json: &str) -> Result<Self> {
        let manifest: PluginManifest =
            serde_json::from_str(json).wrap_err("failed to parse manifest JSON")?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Parse a manifest from a file path.
    pub fn from_file(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .wrap_err_with(|| format!("failed to read manifest at {}", path.display()))?;
        Self::from_json(&contents)
    }

    /// Resolve the effective plugin type.
    ///
    /// If the manifest has an explicit `type` field, return that.
    /// Otherwise infer: if `tools` is non-empty → Tool, else Skill.
    pub fn effective_type(&self) -> PluginType {
        if let Some(ref t) = self.plugin_type {
            return t.clone();
        }
        if !self.tools.is_empty() {
            PluginType::Tool
        } else {
            PluginType::Skill
        }
    }

    /// Validate required fields and internal consistency.
    fn validate(&self) -> Result<()> {
        if self.id.is_empty() {
            bail!("manifest 'id' (or 'name') must not be empty");
        }
        if self.version.is_empty() {
            bail!("manifest 'version' must not be empty");
        }
        // If type is explicitly Tool, must have at least one tool.
        if self.plugin_type == Some(PluginType::Tool) && self.tools.is_empty() {
            bail!(
                "manifest type is 'tool' but no tools are defined for plugin '{}'",
                self.id
            );
        }
        // If type is explicitly Hook, must have at least one hook event.
        if self.plugin_type == Some(PluginType::Hook) && self.hooks.is_empty() {
            bail!(
                "manifest type is 'hook' but no hooks are defined for plugin '{}'",
                self.id
            );
        }
        // Each tool must have a name.
        for tool in &self.tools {
            if tool.name.is_empty() {
                bail!("tool in plugin '{}' has an empty name", self.id);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tool_manifest() {
        let json = r#"{
            "id": "weather",
            "version": "1.0.0",
            "type": "tool",
            "description": "Weather forecasts",
            "tools": [
                {
                    "name": "get_weather",
                    "description": "Get current weather",
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "city": { "type": "string" }
                        },
                        "required": ["city"]
                    }
                }
            ],
            "requires": {
                "env": ["WEATHER_API_KEY"]
            }
        }"#;
        let m = PluginManifest::from_json(json).unwrap();
        assert_eq!(m.id, "weather");
        assert_eq!(m.effective_type(), PluginType::Tool);
        assert_eq!(m.tools.len(), 1);
        assert_eq!(m.tools[0].name, "get_weather");
        let reqs = m.requires.as_ref().unwrap();
        assert_eq!(reqs.env, vec!["WEATHER_API_KEY"]);
    }

    #[test]
    fn parse_skill_manifest() {
        let json = r#"{
            "id": "git-workflow",
            "version": "1.0.0",
            "type": "skill",
            "description": "Git branching best practices",
            "requires": {
                "bins": ["git", "gh"]
            }
        }"#;
        let m = PluginManifest::from_json(json).unwrap();
        assert_eq!(m.id, "git-workflow");
        assert_eq!(m.effective_type(), PluginType::Skill);
        assert!(m.tools.is_empty());
        let reqs = m.requires.as_ref().unwrap();
        assert_eq!(reqs.bins, vec!["git", "gh"]);
    }

    #[test]
    fn parse_hook_manifest() {
        let json = r#"{
            "id": "audit-logger",
            "version": "1.0.0",
            "type": "hook",
            "binary": "main",
            "hooks": ["before_tool_call", "after_tool_call"]
        }"#;
        let m = PluginManifest::from_json(json).unwrap();
        assert_eq!(m.effective_type(), PluginType::Hook);
        assert_eq!(m.hooks.len(), 2);
    }

    #[test]
    fn parse_channel_manifest() {
        let json = r#"{
            "id": "matrix",
            "version": "1.0.0",
            "type": "channel",
            "binary": "main",
            "config_schema": {
                "type": "object",
                "properties": {
                    "homeserver": { "type": "string" }
                }
            }
        }"#;
        let m = PluginManifest::from_json(json).unwrap();
        assert_eq!(m.effective_type(), PluginType::Channel);
        assert!(m.config_schema.is_some());
    }

    #[test]
    fn legacy_name_field_maps_to_id() {
        let json = r#"{
            "name": "news",
            "version": "1.0.0",
            "description": "Fetches news",
            "tools": [
                {
                    "name": "news_fetch",
                    "description": "Fetch news",
                    "entrypoint": "target/release/news_fetch",
                    "input_schema": {}
                }
            ]
        }"#;
        let m = PluginManifest::from_json(json).unwrap();
        assert_eq!(m.id, "news");
        // No explicit type → inferred as Tool because tools is non-empty
        assert_eq!(m.effective_type(), PluginType::Tool);
    }

    #[test]
    fn infer_skill_type_when_no_tools() {
        let json = r#"{
            "id": "my-skill",
            "version": "0.1.0"
        }"#;
        let m = PluginManifest::from_json(json).unwrap();
        assert_eq!(m.effective_type(), PluginType::Skill);
    }

    #[test]
    fn reject_empty_id() {
        let json = r#"{ "id": "", "version": "1.0.0" }"#;
        assert!(PluginManifest::from_json(json).is_err());
    }

    #[test]
    fn reject_empty_version() {
        let json = r#"{ "id": "foo", "version": "" }"#;
        assert!(PluginManifest::from_json(json).is_err());
    }

    #[test]
    fn reject_tool_type_without_tools() {
        let json = r#"{ "id": "foo", "version": "1.0.0", "type": "tool" }"#;
        assert!(PluginManifest::from_json(json).is_err());
    }

    #[test]
    fn reject_hook_type_without_hooks() {
        let json = r#"{ "id": "foo", "version": "1.0.0", "type": "hook" }"#;
        assert!(PluginManifest::from_json(json).is_err());
    }

    #[test]
    fn parse_install_specs() {
        let json = r#"{
            "id": "foo",
            "version": "1.0.0",
            "install": [
                {
                    "id": "brew-foo",
                    "kind": "brew",
                    "formula": "foo",
                    "bins": ["foo"],
                    "os": ["darwin"]
                },
                {
                    "kind": "apt",
                    "package": "foo",
                    "bins": ["foo"],
                    "os": ["linux"]
                }
            ]
        }"#;
        let m = PluginManifest::from_json(json).unwrap();
        assert_eq!(m.install.len(), 2);
        assert_eq!(m.install[0].kind, "brew");
        assert_eq!(m.install[0].formula.as_deref(), Some("foo"));
        assert_eq!(m.install[1].kind, "apt");
    }

    #[test]
    fn parse_existing_weather_manifest() {
        // Matches the actual weather/manifest.json in the repo
        let json = r#"{
            "name": "weather",
            "version": "1.0.0",
            "author": "hagency",
            "description": "Get current weather for any city worldwide via Open-Meteo (free, no API key)",
            "timeout_secs": 15,
            "requires_network": true,
            "tools": [
                {
                    "name": "get_weather",
                    "description": "Get the current weather for a city.",
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "city": { "type": "string" }
                        },
                        "required": ["city"]
                    }
                }
            ]
        }"#;
        let m = PluginManifest::from_json(json).unwrap();
        assert_eq!(m.id, "weather");
        assert_eq!(m.timeout_secs, Some(15));
        assert_eq!(m.requires_network, Some(true));
        assert_eq!(m.effective_type(), PluginType::Tool);
    }
}
