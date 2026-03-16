//! Persistent per-tool configuration store.
//!
//! Allows users to customize tool defaults (language, result count, timeouts, etc.)
//! via chat commands. Settings persist in `{data_dir}/tool_config.json`.
//!
//! Priority chain: explicit per-call args > ToolConfigStore values > hardcoded defaults.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use serde_json::Value;
use tokio::sync::RwLock;
use tracing::warn;

use super::{Tool, ToolResult};

// ---------------------------------------------------------------------------
// ConfigField schema
// ---------------------------------------------------------------------------

/// Describes one configurable field for a tool.
pub struct ConfigField {
    pub key: &'static str,
    pub description: &'static str,
    pub value_type: ConfigValueType,
    pub default_display: &'static str,
}

/// Type constraint for a configurable field.
pub enum ConfigValueType {
    String {
        allowed: Option<&'static [&'static str]>,
    },
    Integer {
        min: i64,
        max: i64,
    },
    Boolean,
}

impl ConfigValueType {
    fn type_name(&self) -> &'static str {
        match self {
            Self::String { .. } => "string",
            Self::Integer { .. } => "integer",
            Self::Boolean => "boolean",
        }
    }

    fn validate(&self, value: &Value) -> std::result::Result<(), String> {
        match self {
            Self::String { allowed } => {
                let s = value
                    .as_str()
                    .ok_or_else(|| "expected a string value".to_string())?;
                if let Some(opts) = allowed {
                    if !opts.contains(&s) {
                        return Err(format!(
                            "invalid value '{}', allowed: {}",
                            s,
                            opts.join(", ")
                        ));
                    }
                }
                Ok(())
            }
            Self::Integer { min, max } => {
                let n = value
                    .as_i64()
                    .ok_or_else(|| "expected an integer value".to_string())?;
                if n < *min || n > *max {
                    return Err(format!("value {} out of range [{}, {}]", n, min, max));
                }
                Ok(())
            }
            Self::Boolean => {
                if value.is_boolean() {
                    Ok(())
                } else {
                    Err("expected a boolean value".to_string())
                }
            }
        }
    }
}

/// Returns the configurable fields for a given tool name, or None if the tool
/// has no configurable fields.
pub fn configurable_fields(tool_name: &str) -> Option<&'static [ConfigField]> {
    match tool_name {
        "news_digest" => Some(&NEWS_DIGEST_FIELDS),
        "deep_crawl" => Some(&DEEP_CRAWL_FIELDS),
        "web_search" => Some(&WEB_SEARCH_FIELDS),
        "web_fetch" => Some(&WEB_FETCH_FIELDS),
        "browser" => Some(&BROWSER_FIELDS),
        _ => None,
    }
}

/// All tool names that have configurable fields.
const CONFIGURABLE_TOOLS: &[&str] = &[
    "news_digest",
    "deep_crawl",
    "web_search",
    "web_fetch",
    "browser",
];

static NEWS_DIGEST_FIELDS: [ConfigField; 6] = [
    ConfigField {
        key: "language",
        description: "Output language",
        value_type: ConfigValueType::String {
            allowed: Some(&["zh", "en"]),
        },
        default_display: "zh",
    },
    ConfigField {
        key: "hn_top_stories",
        description: "HN stories to fetch",
        value_type: ConfigValueType::Integer { min: 5, max: 100 },
        default_display: "30",
    },
    ConfigField {
        key: "max_rss_items",
        description: "Items per RSS feed",
        value_type: ConfigValueType::Integer { min: 5, max: 100 },
        default_display: "30",
    },
    ConfigField {
        key: "max_deep_fetch_total",
        description: "Total articles to deep-fetch",
        value_type: ConfigValueType::Integer { min: 1, max: 50 },
        default_display: "20",
    },
    ConfigField {
        key: "max_source_chars",
        description: "Per-source HTML limit",
        value_type: ConfigValueType::Integer {
            min: 1000,
            max: 50000,
        },
        default_display: "12000",
    },
    ConfigField {
        key: "max_article_chars",
        description: "Per-article content limit",
        value_type: ConfigValueType::Integer {
            min: 1000,
            max: 50000,
        },
        default_display: "8000",
    },
];

static DEEP_CRAWL_FIELDS: [ConfigField; 2] = [
    ConfigField {
        key: "page_settle_ms",
        description: "JS render wait time (ms)",
        value_type: ConfigValueType::Integer {
            min: 500,
            max: 10000,
        },
        default_display: "3000",
    },
    ConfigField {
        key: "max_output_chars",
        description: "Output truncation limit",
        value_type: ConfigValueType::Integer {
            min: 10000,
            max: 200000,
        },
        default_display: "50000",
    },
];

static WEB_SEARCH_FIELDS: [ConfigField; 1] = [ConfigField {
    key: "count",
    description: "Default result count",
    value_type: ConfigValueType::Integer { min: 1, max: 10 },
    default_display: "5",
}];

static WEB_FETCH_FIELDS: [ConfigField; 2] = [
    ConfigField {
        key: "extract_mode",
        description: "Content extraction mode",
        value_type: ConfigValueType::String {
            allowed: Some(&["markdown", "text"]),
        },
        default_display: "markdown",
    },
    ConfigField {
        key: "max_chars",
        description: "Content size limit",
        value_type: ConfigValueType::Integer {
            min: 1000,
            max: 200000,
        },
        default_display: "50000",
    },
];

static BROWSER_FIELDS: [ConfigField; 2] = [
    ConfigField {
        key: "action_timeout_secs",
        description: "Per-action timeout (seconds)",
        value_type: ConfigValueType::Integer { min: 30, max: 600 },
        default_display: "300",
    },
    ConfigField {
        key: "idle_timeout_secs",
        description: "Idle session timeout (seconds)",
        value_type: ConfigValueType::Integer { min: 60, max: 600 },
        default_display: "300",
    },
];

// ---------------------------------------------------------------------------
// ToolConfigStore
// ---------------------------------------------------------------------------

/// Persistent per-tool configuration store backed by JSON file.
pub struct ToolConfigStore {
    path: PathBuf,
    configs: RwLock<HashMap<String, HashMap<String, Value>>>,
}

impl ToolConfigStore {
    /// Open or create the config store at `{data_dir}/tool_config.json`.
    pub async fn open(data_dir: &Path) -> Result<Self> {
        let path = data_dir.join("tool_config.json");
        let configs = if path.exists() {
            let content = tokio::fs::read_to_string(&path)
                .await
                .wrap_err("failed to read tool_config.json")?;
            serde_json::from_str(&content).unwrap_or_else(|e| {
                warn!("failed to parse tool_config.json, starting fresh: {e}");
                HashMap::new()
            })
        } else {
            HashMap::new()
        };
        Ok(Self {
            path,
            configs: RwLock::new(configs),
        })
    }

    /// Read a single setting.
    pub async fn get(&self, tool: &str, key: &str) -> Option<Value> {
        let configs = self.configs.read().await;
        configs.get(tool).and_then(|m| m.get(key)).cloned()
    }

    /// Read all settings for a tool.
    pub async fn get_all(&self, tool: &str) -> Option<HashMap<String, Value>> {
        let configs = self.configs.read().await;
        configs.get(tool).cloned()
    }

    /// Write a setting and persist atomically.
    pub async fn set(&self, tool: &str, key: &str, value: Value) -> Result<()> {
        {
            let mut configs = self.configs.write().await;
            configs
                .entry(tool.to_string())
                .or_default()
                .insert(key.to_string(), value);
        }
        self.persist().await
    }

    /// Delete a setting and persist.
    pub async fn remove(&self, tool: &str, key: &str) -> Result<()> {
        {
            let mut configs = self.configs.write().await;
            if let Some(m) = configs.get_mut(tool) {
                m.remove(key);
                if m.is_empty() {
                    configs.remove(tool);
                }
            }
        }
        self.persist().await
    }

    /// Formatted summary for system prompt injection. Returns empty string if no overrides.
    pub async fn summary(&self) -> String {
        let configs = self.configs.read().await;
        if configs.is_empty() {
            return String::new();
        }
        let mut lines = Vec::new();
        let mut tools: Vec<_> = configs.keys().collect();
        tools.sort();
        for tool in tools {
            if let Some(settings) = configs.get(tool) {
                if settings.is_empty() {
                    continue;
                }
                let mut pairs: Vec<String> = Vec::new();
                let mut keys: Vec<_> = settings.keys().collect();
                keys.sort();
                for key in keys {
                    let val = &settings[key];
                    let display = match val {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    pairs.push(format!("{key}={display}"));
                }
                lines.push(format!("- {}: {}", tool, pairs.join(", ")));
            }
        }
        lines.join("\n")
    }

    // --- Typed helpers ---

    /// Get a string setting.
    pub async fn get_str(&self, tool: &str, key: &str) -> Option<String> {
        self.get(tool, key)
            .await
            .and_then(|v| v.as_str().map(String::from))
    }

    /// Get a u64 setting.
    pub async fn get_u64(&self, tool: &str, key: &str) -> Option<u64> {
        self.get(tool, key).await.and_then(|v| v.as_u64())
    }

    /// Get a usize setting.
    pub async fn get_usize(&self, tool: &str, key: &str) -> Option<usize> {
        self.get_u64(tool, key).await.map(|v| v as usize)
    }

    /// Get a bool setting.
    pub async fn get_bool(&self, tool: &str, key: &str) -> Option<bool> {
        self.get(tool, key).await.and_then(|v| v.as_bool())
    }

    /// Persist configs to disk atomically (write-to-tmp + rename).
    async fn persist(&self) -> Result<()> {
        let configs = self.configs.read().await;
        let json =
            serde_json::to_string_pretty(&*configs).wrap_err("failed to serialize tool config")?;

        // Write to tmp then rename (crash-safe)
        let tmp_path = self.path.with_extension("json.tmp");
        tokio::fs::write(&tmp_path, json.as_bytes())
            .await
            .wrap_err("failed to write tool_config.json.tmp")?;
        tokio::fs::rename(&tmp_path, &self.path)
            .await
            .wrap_err("failed to rename tool_config.json")?;
        Ok(())
    }

    // --- /config slash command handler ---

    /// Handle a `/config` slash command. Returns formatted output string.
    ///
    /// Syntax:
    /// - `/config`                              — list all overrides
    /// - `/config <tool>`                       — show one tool's settings
    /// - `/config set <tool>.<key> <value>`     — set a value
    /// - `/config reset <tool>.<key>`           — reset to default
    pub async fn handle_config_command(&self, args: &str) -> String {
        let args = args.trim();

        // /config  (no args) — list all
        if args.is_empty() {
            return self.cmd_list_all().await;
        }

        let parts: Vec<&str> = args.splitn(3, ' ').collect();

        match parts[0] {
            "set" => {
                if parts.len() < 3 {
                    return "Usage: /config set <tool>.<key> <value>".to_string();
                }
                let (tool, key) = match parse_dot_key(parts[1]) {
                    Some(pair) => pair,
                    None => {
                        return format!(
                            "Invalid key '{}'. Use <tool>.<key> format, e.g. news_digest.language",
                            parts[1]
                        );
                    }
                };
                self.cmd_set(&tool, &key, parts[2]).await
            }
            "reset" => {
                if parts.len() < 2 {
                    return "Usage: /config reset <tool>.<key>".to_string();
                }
                let (tool, key) = match parse_dot_key(parts[1]) {
                    Some(pair) => pair,
                    None => {
                        return format!(
                            "Invalid key '{}'. Use <tool>.<key> format, e.g. news_digest.language",
                            parts[1]
                        );
                    }
                };
                self.cmd_reset(&tool, &key).await
            }
            // /config <tool> — show one tool
            other => self.cmd_show_tool(other).await,
        }
    }

    async fn cmd_list_all(&self) -> String {
        let configs = self.configs.read().await;
        let mut out = String::from("Tool Configuration\n\n");

        for &tool_name in CONFIGURABLE_TOOLS {
            let fields = configurable_fields(tool_name).unwrap();
            let current = configs.get(tool_name);
            let has_overrides = current.is_some_and(|m| !m.is_empty());

            out.push_str(&format!(
                "{}{}:\n",
                tool_name,
                if has_overrides { " *" } else { "" }
            ));

            for field in fields {
                let override_val = current.and_then(|m| m.get(field.key)).map(format_value);

                if let Some(ref val) = override_val {
                    out.push_str(&format!(
                        "  {}: {} (default: {})\n",
                        field.key, val, field.default_display
                    ));
                } else {
                    out.push_str(&format!(
                        "  {}: {} (default)\n",
                        field.key, field.default_display
                    ));
                }
            }
            out.push('\n');
        }
        out
    }

    async fn cmd_show_tool(&self, tool: &str) -> String {
        let fields = match configurable_fields(tool) {
            Some(f) => f,
            None => {
                return format!(
                    "Unknown tool '{}'. Configurable tools: {}",
                    tool,
                    CONFIGURABLE_TOOLS.join(", ")
                );
            }
        };

        let configs = self.configs.read().await;
        let current = configs.get(tool);
        let mut out = format!("{} settings:\n\n", tool);

        for field in fields {
            let override_val = current.and_then(|m| m.get(field.key)).map(format_value);

            let type_info = match &field.value_type {
                ConfigValueType::String {
                    allowed: Some(opts),
                } => format!(" [{}]", opts.join("|")),
                ConfigValueType::Integer { min, max } => format!(" [{min}–{max}]"),
                ConfigValueType::Boolean => " [true|false]".to_string(),
                ConfigValueType::String { allowed: None } => String::new(),
            };

            if let Some(ref val) = override_val {
                out.push_str(&format!(
                    "  {}{}: {} (default: {}) — {}\n",
                    field.key, type_info, val, field.default_display, field.description
                ));
            } else {
                out.push_str(&format!(
                    "  {}{}: {} (default) — {}\n",
                    field.key, type_info, field.default_display, field.description
                ));
            }
        }
        out
    }

    async fn cmd_set(&self, tool: &str, key: &str, raw_value: &str) -> String {
        let fields = match configurable_fields(tool) {
            Some(f) => f,
            None => {
                return format!(
                    "Unknown tool '{}'. Configurable tools: {}",
                    tool,
                    CONFIGURABLE_TOOLS.join(", ")
                );
            }
        };

        let field = match fields.iter().find(|f| f.key == key) {
            Some(f) => f,
            None => {
                let valid: Vec<&str> = fields.iter().map(|f| f.key).collect();
                return format!(
                    "Unknown key '{}' for {}. Valid keys: {}",
                    key,
                    tool,
                    valid.join(", ")
                );
            }
        };

        // Parse raw_value according to the field type
        let value = match &field.value_type {
            ConfigValueType::String { .. } => Value::String(raw_value.to_string()),
            ConfigValueType::Integer { .. } => match raw_value.parse::<i64>() {
                Ok(n) => Value::from(n),
                Err(_) => {
                    return format!("Expected integer for {}.{}, got '{}'", tool, key, raw_value);
                }
            },
            ConfigValueType::Boolean => match raw_value {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                _ => {
                    return format!(
                        "Expected true/false for {}.{}, got '{}'",
                        tool, key, raw_value
                    );
                }
            },
        };

        // Validate range/allowed values
        if let Err(msg) = field.value_type.validate(&value) {
            return format!("Invalid value for {}.{}: {}", tool, key, msg);
        }

        if let Err(e) = self.set(tool, key, value).await {
            return format!("Failed to save: {e}");
        }

        format!("Set {}.{} = {}", tool, key, raw_value)
    }

    async fn cmd_reset(&self, tool: &str, key: &str) -> String {
        let fields = match configurable_fields(tool) {
            Some(f) => f,
            None => {
                return format!(
                    "Unknown tool '{}'. Configurable tools: {}",
                    tool,
                    CONFIGURABLE_TOOLS.join(", ")
                );
            }
        };

        let field = match fields.iter().find(|f| f.key == key) {
            Some(f) => f,
            None => {
                let valid: Vec<&str> = fields.iter().map(|f| f.key).collect();
                return format!(
                    "Unknown key '{}' for {}. Valid keys: {}",
                    key,
                    tool,
                    valid.join(", ")
                );
            }
        };

        if let Err(e) = self.remove(tool, key).await {
            return format!("Failed to save: {e}");
        }

        format!(
            "Reset {}.{} to default ({})",
            tool, key, field.default_display
        )
    }
}

/// Parse "tool.key" into (tool, key). Returns None if no dot found.
fn parse_dot_key(s: &str) -> Option<(String, String)> {
    let dot = s.find('.')?;
    if dot == 0 || dot == s.len() - 1 {
        return None;
    }
    Some((s[..dot].to_string(), s[dot + 1..].to_string()))
}

/// Format a JSON value for display.
fn format_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// ConfigureToolTool
// ---------------------------------------------------------------------------

/// Tool the agent calls to list/get/set/reset user preferences for tools.
pub struct ConfigureToolTool {
    store: Arc<ToolConfigStore>,
}

impl ConfigureToolTool {
    pub fn new(store: Arc<ToolConfigStore>) -> Self {
        Self { store }
    }
}

#[derive(serde::Deserialize)]
struct ConfigureInput {
    action: String,
    #[serde(default)]
    tool: Option<String>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    value: Option<Value>,
}

#[async_trait]
impl Tool for ConfigureToolTool {
    fn name(&self) -> &str {
        "configure_tool"
    }

    fn description(&self) -> &str {
        "View or change default settings for tools. Persists across conversations. \
         Actions: 'list' all configurable tools, 'get' settings for one tool, \
         'set' a key/value, 'reset' a key to its default."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "get", "set", "reset"],
                    "description": "Action: list all tools, get tool settings, set a value, reset to default"
                },
                "tool": {
                    "type": "string",
                    "description": "Tool name (required for get/set/reset)"
                },
                "key": {
                    "type": "string",
                    "description": "Setting key (required for set/reset)"
                },
                "value": {
                    "description": "Value to set (required for set)"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        let input: ConfigureInput =
            serde_json::from_value(args.clone()).wrap_err("invalid configure_tool input")?;

        match input.action.as_str() {
            "list" => self.action_list().await,
            "get" => self.action_get(input.tool.as_deref()).await,
            "set" => {
                self.action_set(
                    input.tool.as_deref(),
                    input.key.as_deref(),
                    input.value.as_ref(),
                )
                .await
            }
            "reset" => {
                self.action_reset(input.tool.as_deref(), input.key.as_deref())
                    .await
            }
            other => Ok(ToolResult {
                output: format!("Unknown action '{}'. Valid: list, get, set, reset", other),
                success: false,
                ..Default::default()
            }),
        }
    }
}

impl ConfigureToolTool {
    async fn action_list(&self) -> Result<ToolResult> {
        let mut output = String::from("# Configurable Tools\n\n");

        for &tool_name in CONFIGURABLE_TOOLS {
            let fields = configurable_fields(tool_name).unwrap();
            output.push_str(&format!("## {}\n", tool_name));

            let current = self.store.get_all(tool_name).await;

            for field in fields {
                let current_val =
                    current
                        .as_ref()
                        .and_then(|m| m.get(field.key))
                        .map(|v| match v {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        });

                let type_info = match &field.value_type {
                    ConfigValueType::String {
                        allowed: Some(opts),
                    } => {
                        format!("string: {}", opts.join("|"))
                    }
                    ConfigValueType::String { allowed: None } => "string".to_string(),
                    ConfigValueType::Integer { min, max } => format!("int: {}–{}", min, max),
                    ConfigValueType::Boolean => "bool".to_string(),
                };

                output.push_str(&format!(
                    "- **{}** ({}) — {} [default: {}]",
                    field.key, type_info, field.description, field.default_display
                ));
                if let Some(ref val) = current_val {
                    output.push_str(&format!(" **(current: {})**", val));
                }
                output.push('\n');
            }
            output.push('\n');
        }

        Ok(ToolResult {
            output,
            success: true,
            ..Default::default()
        })
    }

    async fn action_get(&self, tool: Option<&str>) -> Result<ToolResult> {
        let tool = match tool {
            Some(t) => t,
            None => {
                return Ok(ToolResult {
                    output: "'tool' parameter is required for 'get' action".to_string(),
                    success: false,
                    ..Default::default()
                });
            }
        };

        let fields = match configurable_fields(tool) {
            Some(f) => f,
            None => {
                return Ok(ToolResult {
                    output: format!(
                        "Tool '{}' has no configurable fields. Configurable tools: {}",
                        tool,
                        CONFIGURABLE_TOOLS.join(", ")
                    ),
                    success: false,
                    ..Default::default()
                });
            }
        };

        let current = self.store.get_all(tool).await;
        let mut output = format!("# {} settings\n\n", tool);

        for field in fields {
            let current_val = current.as_ref().and_then(|m| m.get(field.key));
            let display = current_val
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .unwrap_or_else(|| format!("{} (default)", field.default_display));

            output.push_str(&format!(
                "- **{}**: {} — {}\n",
                field.key, display, field.description
            ));
        }

        Ok(ToolResult {
            output,
            success: true,
            ..Default::default()
        })
    }

    async fn action_set(
        &self,
        tool: Option<&str>,
        key: Option<&str>,
        value: Option<&Value>,
    ) -> Result<ToolResult> {
        let tool = match tool {
            Some(t) => t,
            None => {
                return Ok(ToolResult {
                    output: "'tool' parameter is required for 'set' action".to_string(),
                    success: false,
                    ..Default::default()
                });
            }
        };

        let key = match key {
            Some(k) => k,
            None => {
                return Ok(ToolResult {
                    output: "'key' parameter is required for 'set' action".to_string(),
                    success: false,
                    ..Default::default()
                });
            }
        };

        let value = match value {
            Some(v) => v,
            None => {
                return Ok(ToolResult {
                    output: "'value' parameter is required for 'set' action".to_string(),
                    success: false,
                    ..Default::default()
                });
            }
        };

        let fields = match configurable_fields(tool) {
            Some(f) => f,
            None => {
                return Ok(ToolResult {
                    output: format!(
                        "Tool '{}' has no configurable fields. Configurable tools: {}",
                        tool,
                        CONFIGURABLE_TOOLS.join(", ")
                    ),
                    success: false,
                    ..Default::default()
                });
            }
        };

        let field = match fields.iter().find(|f| f.key == key) {
            Some(f) => f,
            None => {
                let valid_keys: Vec<&str> = fields.iter().map(|f| f.key).collect();
                return Ok(ToolResult {
                    output: format!(
                        "Unknown key '{}' for tool '{}'. Valid keys: {}",
                        key,
                        tool,
                        valid_keys.join(", ")
                    ),
                    success: false,
                    ..Default::default()
                });
            }
        };

        if let Err(msg) = field.value_type.validate(value) {
            return Ok(ToolResult {
                output: format!("Invalid value for {}.{}: {}", tool, key, msg),
                success: false,
                ..Default::default()
            });
        }

        self.store.set(tool, key, value.clone()).await?;

        let display = match value {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };

        Ok(ToolResult {
            output: format!(
                "Set {}.{} = {} (was: {}, type: {})",
                tool,
                key,
                display,
                field.default_display,
                field.value_type.type_name()
            ),
            success: true,
            ..Default::default()
        })
    }

    async fn action_reset(&self, tool: Option<&str>, key: Option<&str>) -> Result<ToolResult> {
        let tool = match tool {
            Some(t) => t,
            None => {
                return Ok(ToolResult {
                    output: "'tool' parameter is required for 'reset' action".to_string(),
                    success: false,
                    ..Default::default()
                });
            }
        };

        let key = match key {
            Some(k) => k,
            None => {
                return Ok(ToolResult {
                    output: "'key' parameter is required for 'reset' action".to_string(),
                    success: false,
                    ..Default::default()
                });
            }
        };

        let fields = match configurable_fields(tool) {
            Some(f) => f,
            None => {
                return Ok(ToolResult {
                    output: format!(
                        "Tool '{}' has no configurable fields. Configurable tools: {}",
                        tool,
                        CONFIGURABLE_TOOLS.join(", ")
                    ),
                    success: false,
                    ..Default::default()
                });
            }
        };

        let field = match fields.iter().find(|f| f.key == key) {
            Some(f) => f,
            None => {
                let valid_keys: Vec<&str> = fields.iter().map(|f| f.key).collect();
                return Ok(ToolResult {
                    output: format!(
                        "Unknown key '{}' for tool '{}'. Valid keys: {}",
                        key,
                        tool,
                        valid_keys.join(", ")
                    ),
                    success: false,
                    ..Default::default()
                });
            }
        };

        self.store.remove(tool, key).await?;

        Ok(ToolResult {
            output: format!(
                "Reset {}.{} to default ({})",
                tool, key, field.default_display
            ),
            success: true,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_configurable_fields_known_tools() {
        for &tool in CONFIGURABLE_TOOLS {
            assert!(
                configurable_fields(tool).is_some(),
                "missing fields for {tool}"
            );
        }
    }

    #[test]
    fn test_configurable_fields_unknown_tool() {
        assert!(configurable_fields("unknown_tool").is_none());
    }

    #[test]
    fn test_validate_string_with_allowed() {
        let vt = ConfigValueType::String {
            allowed: Some(&["zh", "en"]),
        };
        assert!(vt.validate(&Value::String("zh".into())).is_ok());
        assert!(vt.validate(&Value::String("en".into())).is_ok());
        assert!(vt.validate(&Value::String("fr".into())).is_err());
        assert!(vt.validate(&Value::from(42)).is_err());
    }

    #[test]
    fn test_validate_integer_range() {
        let vt = ConfigValueType::Integer { min: 5, max: 100 };
        assert!(vt.validate(&Value::from(5)).is_ok());
        assert!(vt.validate(&Value::from(50)).is_ok());
        assert!(vt.validate(&Value::from(100)).is_ok());
        assert!(vt.validate(&Value::from(4)).is_err());
        assert!(vt.validate(&Value::from(101)).is_err());
        assert!(vt.validate(&Value::String("nope".into())).is_err());
    }

    #[test]
    fn test_validate_boolean() {
        let vt = ConfigValueType::Boolean;
        assert!(vt.validate(&Value::Bool(true)).is_ok());
        assert!(vt.validate(&Value::Bool(false)).is_ok());
        assert!(vt.validate(&Value::from(1)).is_err());
    }

    #[tokio::test]
    async fn test_store_roundtrip() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ToolConfigStore::open(dir.path()).await.unwrap();

        // Initially empty
        assert!(store.get("news_digest", "language").await.is_none());
        assert!(store.summary().await.is_empty());

        // Set a value
        store
            .set("news_digest", "language", Value::String("en".into()))
            .await
            .unwrap();

        assert_eq!(
            store.get_str("news_digest", "language").await,
            Some("en".to_string())
        );

        // Summary
        let summary = store.summary().await;
        assert!(summary.contains("news_digest"));
        assert!(summary.contains("language=en"));

        // Persist and reopen
        let store2 = ToolConfigStore::open(dir.path()).await.unwrap();
        assert_eq!(
            store2.get_str("news_digest", "language").await,
            Some("en".to_string())
        );

        // Remove
        store2.remove("news_digest", "language").await.unwrap();
        assert!(store2.get("news_digest", "language").await.is_none());
        assert!(store2.summary().await.is_empty());
    }

    #[tokio::test]
    async fn test_store_typed_helpers() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ToolConfigStore::open(dir.path()).await.unwrap();

        store
            .set("web_search", "count", Value::from(10))
            .await
            .unwrap();
        assert_eq!(store.get_u64("web_search", "count").await, Some(10));
        assert_eq!(store.get_usize("web_search", "count").await, Some(10));
    }

    #[tokio::test]
    async fn test_configure_tool_list() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Arc::new(ToolConfigStore::open(dir.path()).await.unwrap());
        let tool = ConfigureToolTool::new(store);

        let result = tool
            .execute(&serde_json::json!({"action": "list"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("news_digest"));
        assert!(result.output.contains("web_search"));
        assert!(result.output.contains("browser"));
    }

    #[tokio::test]
    async fn test_configure_tool_set_and_get() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Arc::new(ToolConfigStore::open(dir.path()).await.unwrap());
        let tool = ConfigureToolTool::new(store);

        // Set
        let result = tool
            .execute(&serde_json::json!({
                "action": "set",
                "tool": "news_digest",
                "key": "language",
                "value": "en"
            }))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("en"));

        // Get
        let result = tool
            .execute(&serde_json::json!({
                "action": "get",
                "tool": "news_digest"
            }))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("en"));
    }

    #[tokio::test]
    async fn test_configure_tool_validation() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Arc::new(ToolConfigStore::open(dir.path()).await.unwrap());
        let tool = ConfigureToolTool::new(store);

        // Invalid value
        let result = tool
            .execute(&serde_json::json!({
                "action": "set",
                "tool": "news_digest",
                "key": "language",
                "value": "fr"
            }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.output.contains("invalid value"));

        // Unknown key
        let result = tool
            .execute(&serde_json::json!({
                "action": "set",
                "tool": "news_digest",
                "key": "nonexistent",
                "value": "x"
            }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.output.contains("Unknown key"));

        // Unknown tool
        let result = tool
            .execute(&serde_json::json!({
                "action": "get",
                "tool": "nonexistent_tool"
            }))
            .await
            .unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn test_configure_tool_reset() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Arc::new(ToolConfigStore::open(dir.path()).await.unwrap());
        let tool = ConfigureToolTool::new(store.clone());

        // Set then reset
        tool.execute(&serde_json::json!({
            "action": "set",
            "tool": "web_search",
            "key": "count",
            "value": 10
        }))
        .await
        .unwrap();

        assert_eq!(store.get_u64("web_search", "count").await, Some(10));

        let result = tool
            .execute(&serde_json::json!({
                "action": "reset",
                "tool": "web_search",
                "key": "count"
            }))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("default"));

        assert!(store.get("web_search", "count").await.is_none());
    }

    #[test]
    fn test_parse_dot_key() {
        assert_eq!(
            parse_dot_key("news_digest.language"),
            Some(("news_digest".into(), "language".into()))
        );
        assert_eq!(
            parse_dot_key("web_search.count"),
            Some(("web_search".into(), "count".into()))
        );
        assert_eq!(parse_dot_key("nokey"), None);
        assert_eq!(parse_dot_key(".leading"), None);
        assert_eq!(parse_dot_key("trailing."), None);
    }

    #[tokio::test]
    async fn test_config_command_list() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ToolConfigStore::open(dir.path()).await.unwrap();
        let out = store.handle_config_command("").await;
        assert!(out.contains("news_digest"));
        assert!(out.contains("web_search"));
        assert!(out.contains("language"));
    }

    #[tokio::test]
    async fn test_config_command_show_tool() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ToolConfigStore::open(dir.path()).await.unwrap();
        let out = store.handle_config_command("news_digest").await;
        assert!(out.contains("language"));
        assert!(out.contains("zh"));
    }

    #[tokio::test]
    async fn test_config_command_set_and_reset() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ToolConfigStore::open(dir.path()).await.unwrap();

        let out = store
            .handle_config_command("set news_digest.language en")
            .await;
        assert!(out.contains("Set news_digest.language = en"));
        assert_eq!(
            store.get_str("news_digest", "language").await,
            Some("en".into())
        );

        let out = store
            .handle_config_command("reset news_digest.language")
            .await;
        assert!(out.contains("Reset"));
        assert!(store.get("news_digest", "language").await.is_none());
    }

    #[tokio::test]
    async fn test_config_command_set_integer() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ToolConfigStore::open(dir.path()).await.unwrap();

        let out = store.handle_config_command("set web_search.count 8").await;
        assert!(out.contains("Set web_search.count = 8"));
        assert_eq!(store.get_u64("web_search", "count").await, Some(8));
    }

    #[tokio::test]
    async fn test_config_command_validation_errors() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ToolConfigStore::open(dir.path()).await.unwrap();

        // Invalid allowed value
        let out = store
            .handle_config_command("set news_digest.language fr")
            .await;
        assert!(out.contains("invalid value"));

        // Out of range
        let out = store.handle_config_command("set web_search.count 99").await;
        assert!(out.contains("out of range"));

        // Unknown tool
        let out = store.handle_config_command("fake_tool").await;
        assert!(out.contains("Unknown tool"));

        // Bad dot key
        let out = store.handle_config_command("set nokey 5").await;
        assert!(out.contains("Use <tool>.<key>"));
    }
}
