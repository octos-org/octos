//! Dora-RS to MCP tool bridge — canonical home for the HTTP-transport
//! bridge tool.
//!
//! Wraps a [`DoraToolMapping`] (a config-defined link from a Dora node output
//! to an MCP tool name) so the agent's [`Tool`] machinery can dispatch the
//! request like any other tool. The bridge also registers the tool's
//! required safety tier in the global [`RobotToolRegistry`] so the existing
//! `group:robot:<tier>` `ToolPolicy` machinery actually applies — without
//! that registration the tier metadata would be decorative.
//!
//! This module is the canonical location. The historical `octos-dora-mcp`
//! crate now re-exports these items for backward compatibility; new code
//! should depend on `octos-agent` directly and use
//! `octos_agent::tools::dora_bridge::*`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::permissions::SafetyTier;
use crate::tools::HttpTool;
use crate::tools::robot_groups::{self, RobotToolRegistry};
use crate::tools::{ConcurrencyClass, Tool, ToolResult};

/// Default timeout (seconds) for a bridge tool call.
pub fn default_timeout() -> u64 {
    30
}

/// Mapping from a catalog entry to an MCP tool exposed via HTTP bridge.
///
/// The post-SPEC-V1 1.1 shape: tools are dispatched via `POST <bridge_base_url>/tools/<tool_name>`
/// and the bridge handles all Dora wiring. The legacy `dora_node_id` /
/// `dora_output_id` fields remain optional for backward-compatible config
/// files but are ignored by the HTTP transport.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DoraToolMapping {
    /// MCP tool name exposed to the agent.
    pub tool_name: String,
    /// Description for the LLM.
    #[serde(default)]
    pub description: String,
    /// Loopback HTTP base URL of the local bridge process (`http://127.0.0.1:PORT`).
    /// Required for execution; validated against loopback at bridge build time.
    #[serde(default)]
    pub bridge_base_url: String,
    /// Required safety tier for this tool. Strict: unknown tiers fail
    /// `BridgeConfig::from_json` rather than silently defaulting to `Observe`.
    #[serde(default)]
    pub safety_tier: SafetyTier,
    /// Optional explicit override of the bridge tool's concurrency class.
    /// When `None`, falls back to the per-tier default in
    /// `Tool::concurrency_class` (Observe→Safe, anything else→Exclusive).
    #[serde(default)]
    pub concurrency_class: Option<ConcurrencyClass>,
    /// Optional JSON Schema for the tool's input. When `None`, a permissive
    /// `{"type":"object","additionalProperties":true}` is used.
    #[serde(default)]
    pub input_schema: Option<serde_json::Value>,
    /// Timeout in seconds for the tool call.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// DEPRECATED — kept for older config compatibility but ignored by the
    /// HTTP-transport implementation. Will be removed in a future release.
    #[serde(default)]
    pub dora_node_id: Option<String>,
    /// DEPRECATED — see `dora_node_id`.
    #[serde(default)]
    pub dora_output_id: Option<String>,
    /// Optional SPEC-V1 verb path used on the bridge URL. When `None`,
    /// `tool_name` is used (back-compat). Set this when `tool_name` is the
    /// LLM-facing sanitized form (e.g., `vendor_agibot_a2_motion_set_action`)
    /// but the bridge expects the original dotted verb on its URL (e.g.,
    /// `vendor.agibot.a2.motion.set_action`). Both Anthropic and OpenAI tool
    /// APIs require names to match `^[a-zA-Z0-9_-]+$` and reject dots.
    #[serde(default)]
    pub verb_path: Option<String>,
}

/// Sanitize a SPEC-V1 verb (e.g. `vendor.agibot.a2.motion.set_action`) into a
/// name that satisfies the LLM provider tool-name pattern `^[a-zA-Z0-9_-]+$`.
/// Any character outside the allowed set becomes `_`. Empty input returns
/// `_` to keep the result non-empty.
pub fn sanitize_tool_name(raw: &str) -> String {
    if raw.is_empty() {
        return "_".to_string();
    }
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

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

/// A bridge that wraps a [`DoraToolMapping`] as an MCP-compatible [`Tool`].
///
/// Transport is delegated to [`HttpTool`]; this layer owns safety_tier
/// metadata + `RobotToolRegistry` registration so the existing
/// `group:robot:<tier>` `ToolPolicy` machinery applies.
pub struct DoraToolBridge {
    mapping: DoraToolMapping,
    inner: HttpTool,
}

impl DoraToolBridge {
    /// Construct a bridge. Fails if `mapping.bridge_base_url` is not a
    /// loopback HTTP URL (delegated to `HttpTool::new`).
    pub fn new(mapping: DoraToolMapping) -> eyre::Result<Self> {
        let schema = mapping
            .input_schema
            .clone()
            .unwrap_or_else(|| serde_json::json!({"type": "object", "additionalProperties": true}));
        // The HttpTool's `name` doubles as the URL path on the bridge — use
        // `verb_path` when set so the LLM-facing `tool_name` can be a sanitized
        // form (no dots) while dispatch hits the original SPEC-V1 verb.
        let url_path = mapping
            .verb_path
            .clone()
            .unwrap_or_else(|| mapping.tool_name.clone());
        let inner = HttpTool::new(
            url_path,
            mapping.description.clone(),
            mapping.bridge_base_url.clone(),
            schema,
            mapping.timeout_secs,
        )?;
        Ok(Self { mapping, inner })
    }

    /// Return a reference to the underlying mapping.
    pub fn mapping(&self) -> &DoraToolMapping {
        &self.mapping
    }

    /// Required safety tier for this tool, drawn directly from the mapping.
    pub fn required_safety_tier(&self) -> SafetyTier {
        self.mapping.safety_tier
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
        self.inner.input_schema()
    }

    fn tags(&self) -> &[&str] {
        &["dora-bridge", "robot"]
    }

    /// Codex round-2 P2: anything that can actuate the robot must NOT run in
    /// the same parallel batch as another bridge call against the same Dora
    /// runtime — overlapping motion commands are a real safety hazard. Only
    /// `Observe`-tier tools (sensors, status queries) keep the default
    /// parallel-friendly `Safe` class; every higher tier is `Exclusive` so
    /// the agent's batch dispatcher serialises them.
    fn concurrency_class(&self) -> ConcurrencyClass {
        if let Some(explicit) = self.mapping.concurrency_class {
            return explicit;
        }
        match self.mapping.safety_tier {
            SafetyTier::Observe => ConcurrencyClass::Safe,
            SafetyTier::SafeMotion | SafetyTier::FullActuation | SafetyTier::EmergencyOverride => {
                ConcurrencyClass::Exclusive
            }
        }
    }

    async fn execute(&self, args: &serde_json::Value) -> eyre::Result<ToolResult> {
        self.inner.execute(args).await
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
///
/// Returns an error if any mapping has an invalid `bridge_base_url`
/// (non-loopback / malformed) — the caller decides whether to abort.
pub fn load_bridges(config: &BridgeConfig) -> eyre::Result<Vec<DoraToolBridge>> {
    let bridges: Vec<DoraToolBridge> = config
        .mappings
        .iter()
        .map(|m| DoraToolBridge::new(m.clone()))
        .collect::<eyre::Result<Vec<_>>>()?;
    robot_groups::with_registry_mut(|reg: &mut RobotToolRegistry| {
        for bridge in &bridges {
            reg.insert(bridge.mapping.tool_name.clone(), bridge.mapping.safety_tier);
        }
    });
    Ok(bridges)
}
