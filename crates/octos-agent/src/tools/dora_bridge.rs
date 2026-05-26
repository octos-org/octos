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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_mapping() -> DoraToolMapping {
        DoraToolMapping {
            tool_name: "navigate_to".to_string(),
            verb_path: None,
            description: "Navigate robot to a waypoint".to_string(),
            bridge_base_url: "http://127.0.0.1:8765".to_string(),
            safety_tier: SafetyTier::SafeMotion,
            concurrency_class: None,
            input_schema: None,
            timeout_secs: 60,
            dora_node_id: None,
            dora_output_id: None,
        }
    }

    #[test]
    fn sanitize_tool_name_replaces_dots_with_underscores() {
        assert_eq!(
            sanitize_tool_name("vendor.agibot.a2.motion.set_action"),
            "vendor_agibot_a2_motion_set_action",
        );
    }

    #[test]
    fn sanitize_tool_name_preserves_already_clean_names() {
        assert_eq!(sanitize_tool_name("robot_heartbeat"), "robot_heartbeat");
        assert_eq!(sanitize_tool_name("read_file"), "read_file");
        assert_eq!(sanitize_tool_name("set-action"), "set-action");
    }

    #[test]
    fn sanitize_tool_name_handles_empty_input() {
        assert_eq!(sanitize_tool_name(""), "_");
    }

    #[test]
    fn sanitize_tool_name_replaces_arbitrary_disallowed_chars() {
        // Slash, space, colon all become `_`.
        assert_eq!(sanitize_tool_name("foo bar/baz:qux"), "foo_bar_baz_qux");
    }

    #[test]
    fn dora_bridge_uses_verb_path_for_url_when_set() {
        // verb_path is the dotted SPEC-V1 verb; tool_name is sanitized for LLM.
        let mapping = DoraToolMapping {
            tool_name: "vendor_agibot_a2_motion_set_action".to_string(),
            verb_path: Some("vendor.agibot.a2.motion.set_action".to_string()),
            description: "Set action".to_string(),
            bridge_base_url: "http://127.0.0.1:8765".to_string(),
            safety_tier: SafetyTier::SafeMotion,
            concurrency_class: None,
            input_schema: None,
            timeout_secs: 5,
            dora_node_id: None,
            dora_output_id: None,
        };
        let bridge = DoraToolBridge::new(mapping).unwrap();
        // Tool::name() returns the sanitized form shown to the LLM.
        assert_eq!(bridge.name(), "vendor_agibot_a2_motion_set_action");
        // The inner HttpTool uses verb_path for its URL — verify by hitting a
        // wiremock that only answers on the dotted-verb path.
    }

    #[tokio::test]
    async fn dora_bridge_dispatches_to_verb_path_url() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // The bridge must POST to the dotted verb, NOT the sanitized name.
        Mock::given(method("POST"))
            .and(path("/tools/vendor.agibot.a2.motion.set_action"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true, "code": "0", "msg": "", "data": {"applied": true}
            })))
            .mount(&server)
            .await;

        let mapping = DoraToolMapping {
            tool_name: "vendor_agibot_a2_motion_set_action".to_string(),
            verb_path: Some("vendor.agibot.a2.motion.set_action".to_string()),
            description: "Set action".to_string(),
            bridge_base_url: server.uri(),
            safety_tier: SafetyTier::SafeMotion,
            concurrency_class: None,
            input_schema: None,
            timeout_secs: 5,
            dora_node_id: None,
            dora_output_id: None,
        };
        let bridge = DoraToolBridge::new(mapping).unwrap();
        let result = bridge
            .execute(&serde_json::json!({"action": "RL_LOCOMOTION_DEFAULT"}))
            .await
            .unwrap();
        assert!(result.success, "expected dispatch via verb_path to succeed");
    }

    #[test]
    fn should_expose_correct_tool_name() {
        let bridge = DoraToolBridge::new(sample_mapping()).unwrap();
        assert_eq!(bridge.name(), "navigate_to");
    }

    #[test]
    fn should_expose_correct_description() {
        let bridge = DoraToolBridge::new(sample_mapping()).unwrap();
        assert_eq!(bridge.description(), "Navigate robot to a waypoint");
    }

    #[test]
    fn should_include_dora_tags() {
        let bridge = DoraToolBridge::new(sample_mapping()).unwrap();
        let tags = bridge.tags();
        assert!(tags.contains(&"dora-bridge"));
        assert!(tags.contains(&"robot"));
    }

    #[test]
    fn should_expose_declared_safety_tier() {
        let bridge = DoraToolBridge::new(sample_mapping()).unwrap();
        assert_eq!(bridge.required_safety_tier(), SafetyTier::SafeMotion);
    }

    #[test]
    fn concurrency_class_override_supersedes_tier_default() {
        let mut mapping = sample_mapping();
        // sample_mapping is SafeMotion → would default to Exclusive.
        mapping.concurrency_class = Some(ConcurrencyClass::Safe);
        let bridge = DoraToolBridge::new(mapping).unwrap();
        assert!(matches!(bridge.concurrency_class(), ConcurrencyClass::Safe));
    }

    #[test]
    fn concurrency_class_falls_back_to_tier_default_when_unset() {
        let bridge = DoraToolBridge::new(sample_mapping()).unwrap();
        // SafeMotion → Exclusive.
        assert!(matches!(
            bridge.concurrency_class(),
            ConcurrencyClass::Exclusive
        ));
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
                "bridge_base_url": "http://127.0.0.1:8765",
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
        let bridges = load_bridges(&config).unwrap();
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
    async fn execute_forwards_to_bridge_via_http() {
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/robot.heartbeat"))
            .and(body_json(serde_json::json!({"args": {}})))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true, "code": "0", "msg": "", "data": {"ok": true, "deadline_ms": 0}
            })))
            .mount(&server)
            .await;

        let mapping = DoraToolMapping {
            tool_name: "robot.heartbeat".into(),
            verb_path: None,
            description: "heartbeat".into(),
            bridge_base_url: server.uri(),
            safety_tier: SafetyTier::Observe,
            concurrency_class: None,
            timeout_secs: 5,
            input_schema: None,
            dora_node_id: None,
            dora_output_id: None,
        };
        let bridge = DoraToolBridge::new(mapping).unwrap();
        let result = bridge.execute(&serde_json::json!({})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("deadline_ms"));
    }

    #[tokio::test]
    async fn dora_bridge_respects_mapping_timeout() {
        use std::time::Duration;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/slow"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_secs(3))
                    .set_body_json(
                        serde_json::json!({"ok": true, "code": "0", "msg": "", "data": {}}),
                    ),
            )
            .mount(&server)
            .await;

        let mapping = DoraToolMapping {
            tool_name: "slow".into(),
            verb_path: None,
            description: "slow".into(),
            bridge_base_url: server.uri(),
            safety_tier: SafetyTier::Observe,
            input_schema: None,
            timeout_secs: 1, // 1 s < 3 s server delay
            concurrency_class: None,
            dora_node_id: None,
            dora_output_id: None,
        };
        let bridge = DoraToolBridge::new(mapping).unwrap();
        let result = bridge.execute(&serde_json::json!({})).await.unwrap();
        assert!(!result.success, "expected timeout failure, got success");
    }

    // Config-side tests previously in octos-dora-mcp::config::tests —
    // moved here to keep the canonical home complete.

    #[test]
    fn should_parse_config_from_json() {
        let json = r#"{
            "description": "UR5e inspection tools",
            "mappings": [
                {
                    "tool_name": "scan_station",
                    "description": "Scan station for objects",
                    "bridge_base_url": "http://127.0.0.1:8765",
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
                "bridge_base_url": "http://127.0.0.1:8765",
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
        // string fails. Mirrors `SafetyTier::default()`.
        let json = r#"{
            "mappings": [{
                "tool_name": "x",
                "description": "x",
                "bridge_base_url": "http://127.0.0.1:8765",
                "timeout_secs": 1
            }]
        }"#;
        let config = BridgeConfig::from_json(json).unwrap();
        assert_eq!(config.mappings[0].safety_tier, SafetyTier::Observe);
    }
}
