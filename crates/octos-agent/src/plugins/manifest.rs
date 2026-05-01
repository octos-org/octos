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
    /// If true, the tool runs in a background task automatically when called.
    /// The execution loop returns immediately with `spawn_only_message`.
    #[serde(default)]
    pub spawn_only: bool,
    /// Environment variable names this tool is explicitly allowed to receive.
    ///
    /// Secret-like env vars (API keys, passwords, tokens, secrets) are stripped
    /// from plugin subprocesses unless their name is listed here. Non-secret
    /// runtime env vars are still forwarded by default.
    #[serde(default, alias = "env_allowlist")]
    pub env: Vec<String>,
    /// Manifest-declared approval risk for this tool.
    #[serde(default)]
    pub risk: Option<String>,
    /// Message returned to the LLM when a spawn_only tool is auto-backgrounded.
    /// Default: "SUCCESS: Task is now running in background..."
    #[serde(default)]
    pub spawn_only_message: Option<String>,
    /// Item 6 of OCTOS_M8_FIX_FIRST_CHECKLIST_2026-04-24:
    /// optional concurrency class. When `"exclusive"` the M8.8
    /// scheduler serialises this tool against any sibling in the same
    /// batch instead of fanning out in parallel. Default `None` means
    /// the wrapper falls back to `Safe`. Mutating plugin tools should
    /// declare `"exclusive"` to avoid silently inheriting Safe.
    #[serde(default)]
    pub concurrency_class: Option<String>,
}

/// Recognised values for the manifest-declared `risk` field.
///
/// M6 req 4 enforcement (UPCR-2026-001): a tool that declares
/// `risk: "high"` or `risk: "critical"` must trigger an interactive approval
/// prompt before each invocation. `low` is treated as auto-approved.
/// `medium` and any unknown literal fall through to "no enforced gate" — the
/// risk is still surfaced on `approval_requested.risk` for display, but the
/// agent does not synthesise an approval check (intent: medium semantics
/// remain ambiguous; revisit per Tier 2/3 follow-up).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestRiskGate {
    /// Auto-approved — skip the interactive prompt.
    Low,
    /// Ambiguous; surfaced for display, no enforced gate.
    MediumOrUnspecified,
    /// Must request user approval before invocation.
    HighOrCritical,
}

impl ManifestRiskGate {
    /// Classify a manifest risk literal. Whitespace and ASCII case are
    /// ignored. Unknown literals map to [`ManifestRiskGate::MediumOrUnspecified`]
    /// so the agent does not silently strengthen a value the manifest
    /// author did not write.
    pub fn classify(risk: Option<&str>) -> Self {
        match risk
            .map(str::trim)
            .filter(|risk| !risk.is_empty())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("low") => Self::Low,
            Some("high") | Some("critical") => Self::HighOrCritical,
            _ => Self::MediumOrUnspecified,
        }
    }

    /// Whether this risk literal forces an interactive approval prompt.
    pub fn requires_approval(self) -> bool {
        matches!(self, Self::HighOrCritical)
    }
}

/// Manifest-level validation error surfaced at registration time.
///
/// Loader code calls [`PluginToolDef::validate_for_registration`] before
/// wiring the tool into the registry. A returned error means the plugin
/// declares fields the harness cannot enforce safely; the plugin is
/// rejected (loader logs and skips).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestValidationError {
    /// `env` allowlist contains a name that fails the syntactic check.
    /// First field: the offending name; second: human-readable reason.
    InvalidEnvName(String, &'static str),
}

impl std::fmt::Display for ManifestValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidEnvName(name, reason) => write!(
                f,
                "manifest env allowlist entry {name:?} is invalid: {reason}"
            ),
        }
    }
}

impl std::error::Error for ManifestValidationError {}

impl PluginToolDef {
    /// Validate manifest fields whose enforcement gates run at runtime.
    ///
    /// M6 req 4: this is the registration-time half of the env-allowlist
    /// gate. Runtime filtering relies on [`PluginToolDef::env`] being a
    /// list of well-formed env-var names — anything that smells like a
    /// shell-injection token (`=`, control chars) or a known process
    /// hijack vector (`LD_PRELOAD`, `DYLD_*` etc.) is rejected here so a
    /// malicious manifest cannot use the allowlist as a bypass channel.
    pub fn validate_for_registration(&self) -> Result<(), ManifestValidationError> {
        for name in &self.env {
            validate_manifest_env_name(name)?;
        }
        Ok(())
    }

    /// Returns the trimmed/lowercased `concurrency_class` literal if it is
    /// recognised. Returns `None` for missing values; returns
    /// `Some("unknown:...")` for declared-but-unrecognised values so the
    /// loader can warn without rejecting (silent fallback to Safe is the
    /// existing behavior — we don't want to break a manifest that
    /// declares e.g. `"safe"` even though Safe is the default).
    ///
    /// Recognised: `exclusive`, `safe`. Anything else (including
    /// `"medium"`, `"highly-exclusive"`, ...) is reported as unknown so
    /// operators can spot typos like `"exclusive "` (trailing space —
    /// previously silently downgraded to Safe).
    pub fn classify_concurrency_class(&self) -> ConcurrencyClassClassification {
        let Some(raw) = self.concurrency_class.as_deref() else {
            return ConcurrencyClassClassification::Unset;
        };
        let trimmed = raw.trim().to_ascii_lowercase();
        match trimmed.as_str() {
            "" => ConcurrencyClassClassification::Unset,
            "exclusive" => ConcurrencyClassClassification::Exclusive,
            "safe" => ConcurrencyClassClassification::Safe,
            _ => ConcurrencyClassClassification::Unknown(raw.to_string()),
        }
    }
}

/// Result of [`PluginToolDef::classify_concurrency_class`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConcurrencyClassClassification {
    /// No `concurrency_class` declared. Falls back to the trait default
    /// (`Safe`).
    Unset,
    /// Declared `"exclusive"` (post-trim, case-insensitive).
    Exclusive,
    /// Declared `"safe"` (post-trim, case-insensitive). Equivalent to
    /// Unset at runtime but distinguished here so a future tightening
    /// can reject Unset for mutating tools while keeping explicit Safe.
    Safe,
    /// Declared but unrecognised. Carries the original raw value for
    /// diagnostic logging. Runtime behavior falls back to Safe.
    Unknown(String),
}

fn validate_manifest_env_name(name: &str) -> Result<(), ManifestValidationError> {
    if name.is_empty() {
        return Err(ManifestValidationError::InvalidEnvName(
            name.to_string(),
            "empty name",
        ));
    }
    if name.contains('=') {
        return Err(ManifestValidationError::InvalidEnvName(
            name.to_string(),
            "name must not contain '='",
        ));
    }
    if name.chars().any(|ch| ch.is_control() || ch.is_whitespace()) {
        return Err(ManifestValidationError::InvalidEnvName(
            name.to_string(),
            "name must not contain whitespace or control characters",
        ));
    }
    if name.starts_with(|ch: char| ch.is_ascii_digit()) {
        return Err(ManifestValidationError::InvalidEnvName(
            name.to_string(),
            "name must not start with a digit",
        ));
    }
    for ch in name.chars() {
        if !(ch.is_ascii_alphanumeric() || ch == '_') {
            return Err(ManifestValidationError::InvalidEnvName(
                name.to_string(),
                "name must use only [A-Za-z0-9_]",
            ));
        }
    }
    // Reject known process-hijack env names. The same list is stripped
    // unconditionally at subprocess spawn time, but rejecting at
    // registration makes the malicious manifest visible in logs instead
    // of letting it linger as a no-op.
    for blocked in crate::sandbox::BLOCKED_ENV_VARS {
        if name.eq_ignore_ascii_case(blocked) {
            return Err(ManifestValidationError::InvalidEnvName(
                name.to_string(),
                "name is a known process-hijack env var",
            ));
        }
    }
    Ok(())
}

impl PluginToolDef {
    /// Whether this tool's input schema declares it accepts host-injected
    /// config under the named key (e.g. `"synthesis_config"`).
    ///
    /// Schema lookup: the manifest may either list the key under
    /// `input_schema["x-octos-host-config-keys"]` (a string array) or define
    /// it as a property in `input_schema["properties"]`. Either form is
    /// sufficient — having the key in `properties` is what the plugin
    /// actually parses; the `x-octos-host-config-keys` extension is the
    /// explicit opt-in signal so other plugins don't accidentally receive
    /// secrets they didn't declare.
    pub fn accepts_host_config_key(&self, key: &str) -> bool {
        let schema = &self.input_schema;
        // Explicit opt-in via x-octos-host-config-keys.
        if let Some(keys) = schema
            .get("x-octos-host-config-keys")
            .and_then(|v| v.as_array())
        {
            for k in keys {
                if k.as_str() == Some(key) {
                    return true;
                }
            }
        }
        false
    }
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
                    "risk": "medium",
                    "input_schema": {"type": "object", "properties": {"name": {"type": "string"}}}
                }
            ]
        }"#;
        let manifest: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(manifest.name, "test-plugin");
        assert_eq!(manifest.tools.len(), 1);
        assert_eq!(manifest.tools[0].name, "hello");
        assert_eq!(manifest.tools[0].risk.as_deref(), Some("medium"));
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
        assert!(manifest.tools[0].env.is_empty());
        assert_eq!(manifest.tools[0].risk, None);
    }

    #[test]
    fn test_tool_risk_preserves_blank_manifest_value() {
        let json = r#"{
            "name": "risk-plugin",
            "version": "1.0.0",
            "tools": [{"name": "t", "description": "d", "risk": "   "}]
        }"#;
        let manifest: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(manifest.tools[0].risk.as_deref(), Some("   "));
    }

    #[test]
    fn test_tool_env_allowlist() {
        let json = r#"{
            "name": "env-plugin",
            "version": "1.0.0",
            "tools": [{
                "name": "send",
                "description": "Send",
                "env": ["SMTP_PASSWORD", "OPENAI_API_KEY"]
            }]
        }"#;
        let manifest: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(
            manifest.tools[0].env,
            vec!["SMTP_PASSWORD".to_string(), "OPENAI_API_KEY".to_string()]
        );
    }

    #[test]
    fn accepts_host_config_key_returns_false_when_extension_absent() {
        let json = r#"{
            "name": "p",
            "version": "1",
            "tools": [{
                "name": "t",
                "description": "d",
                "input_schema": {"type": "object", "properties": {"q": {"type": "string"}}}
            }]
        }"#;
        let manifest: PluginManifest = serde_json::from_str(json).unwrap();
        assert!(!manifest.tools[0].accepts_host_config_key("synthesis_config"));
    }

    #[test]
    fn accepts_host_config_key_honours_extension_array() {
        let json = r#"{
            "name": "p",
            "version": "1",
            "tools": [{
                "name": "deep_search",
                "description": "Research",
                "input_schema": {
                    "type": "object",
                    "properties": {"q": {"type": "string"}},
                    "x-octos-host-config-keys": ["synthesis_config"]
                }
            }]
        }"#;
        let manifest: PluginManifest = serde_json::from_str(json).unwrap();
        assert!(manifest.tools[0].accepts_host_config_key("synthesis_config"));
        // Other keys still rejected — explicit opt-in only.
        assert!(!manifest.tools[0].accepts_host_config_key("smtp_config"));
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

    fn def_with_env(env: Vec<&str>) -> PluginToolDef {
        PluginToolDef {
            name: "t".to_string(),
            description: "d".to_string(),
            input_schema: default_schema(),
            spawn_only: false,
            env: env.into_iter().map(str::to_string).collect(),
            risk: None,
            spawn_only_message: None,
            concurrency_class: None,
        }
    }

    #[test]
    fn validate_for_registration_accepts_clean_allowlist() {
        let def = def_with_env(vec!["OPENAI_API_KEY", "SMTP_HOST", "_FOO_BAR_"]);
        assert!(def.validate_for_registration().is_ok());
    }

    #[test]
    fn validate_for_registration_accepts_empty_allowlist() {
        let def = def_with_env(vec![]);
        assert!(def.validate_for_registration().is_ok());
    }

    #[test]
    fn validate_for_registration_rejects_empty_entry() {
        let def = def_with_env(vec![""]);
        let err = def.validate_for_registration().unwrap_err();
        assert!(matches!(err, ManifestValidationError::InvalidEnvName(_, _)));
    }

    #[test]
    fn validate_for_registration_rejects_equals_sign() {
        let def = def_with_env(vec!["FOO=bar"]);
        assert!(def.validate_for_registration().is_err());
    }

    #[test]
    fn validate_for_registration_rejects_whitespace() {
        let def = def_with_env(vec!["FOO BAR"]);
        assert!(def.validate_for_registration().is_err());
        let def = def_with_env(vec!["FOO\nBAR"]);
        assert!(def.validate_for_registration().is_err());
    }

    #[test]
    fn validate_for_registration_rejects_leading_digit() {
        let def = def_with_env(vec!["1FOO"]);
        assert!(def.validate_for_registration().is_err());
    }

    #[test]
    fn validate_for_registration_rejects_non_alphanumeric() {
        let def = def_with_env(vec!["FOO-BAR"]);
        assert!(def.validate_for_registration().is_err());
        let def = def_with_env(vec!["FOO.BAR"]);
        assert!(def.validate_for_registration().is_err());
    }

    #[test]
    fn validate_for_registration_rejects_blocked_env_names() {
        // BLOCKED_ENV_VARS includes process-hijack vars like LD_PRELOAD,
        // DYLD_INSERT_LIBRARIES, NODE_OPTIONS, etc.
        let def = def_with_env(vec!["LD_PRELOAD"]);
        assert!(def.validate_for_registration().is_err());
        let def = def_with_env(vec!["DYLD_INSERT_LIBRARIES"]);
        assert!(def.validate_for_registration().is_err());
        // Case-insensitive match.
        let def = def_with_env(vec!["ld_preload"]);
        assert!(def.validate_for_registration().is_err());
    }

    #[test]
    fn manifest_risk_gate_classifies_known_literals() {
        assert_eq!(
            ManifestRiskGate::classify(Some("low")),
            ManifestRiskGate::Low
        );
        assert_eq!(
            ManifestRiskGate::classify(Some("LOW")),
            ManifestRiskGate::Low
        );
        assert_eq!(
            ManifestRiskGate::classify(Some(" Low ")),
            ManifestRiskGate::Low
        );
        assert_eq!(
            ManifestRiskGate::classify(Some("high")),
            ManifestRiskGate::HighOrCritical
        );
        assert_eq!(
            ManifestRiskGate::classify(Some("CRITICAL")),
            ManifestRiskGate::HighOrCritical
        );
    }

    #[test]
    fn manifest_risk_gate_falls_back_for_unknown_or_blank() {
        assert_eq!(
            ManifestRiskGate::classify(None),
            ManifestRiskGate::MediumOrUnspecified
        );
        assert_eq!(
            ManifestRiskGate::classify(Some("")),
            ManifestRiskGate::MediumOrUnspecified
        );
        assert_eq!(
            ManifestRiskGate::classify(Some("   ")),
            ManifestRiskGate::MediumOrUnspecified
        );
        assert_eq!(
            ManifestRiskGate::classify(Some("medium")),
            ManifestRiskGate::MediumOrUnspecified
        );
        assert_eq!(
            ManifestRiskGate::classify(Some("super-critical")),
            ManifestRiskGate::MediumOrUnspecified
        );
    }

    #[test]
    fn manifest_risk_gate_requires_approval_only_for_high_critical() {
        assert!(!ManifestRiskGate::Low.requires_approval());
        assert!(!ManifestRiskGate::MediumOrUnspecified.requires_approval());
        assert!(ManifestRiskGate::HighOrCritical.requires_approval());
    }

    fn def_with_concurrency(class: Option<&str>) -> PluginToolDef {
        PluginToolDef {
            name: "t".to_string(),
            description: "d".to_string(),
            input_schema: default_schema(),
            spawn_only: false,
            env: vec![],
            risk: None,
            spawn_only_message: None,
            concurrency_class: class.map(str::to_string),
        }
    }

    #[test]
    fn classify_concurrency_class_recognises_known_literals() {
        assert_eq!(
            def_with_concurrency(Some("exclusive")).classify_concurrency_class(),
            ConcurrencyClassClassification::Exclusive
        );
        assert_eq!(
            def_with_concurrency(Some("EXCLUSIVE")).classify_concurrency_class(),
            ConcurrencyClassClassification::Exclusive
        );
        // Codex review #1: trailing whitespace must not silently
        // downgrade `"exclusive "` to Safe.
        assert_eq!(
            def_with_concurrency(Some("exclusive ")).classify_concurrency_class(),
            ConcurrencyClassClassification::Exclusive
        );
        assert_eq!(
            def_with_concurrency(Some("safe")).classify_concurrency_class(),
            ConcurrencyClassClassification::Safe
        );
    }

    #[test]
    fn classify_concurrency_class_flags_unknown_literals() {
        match def_with_concurrency(Some("nonsense")).classify_concurrency_class() {
            ConcurrencyClassClassification::Unknown(raw) => assert_eq!(raw, "nonsense"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn classify_concurrency_class_unset_when_missing_or_blank() {
        assert_eq!(
            def_with_concurrency(None).classify_concurrency_class(),
            ConcurrencyClassClassification::Unset
        );
        assert_eq!(
            def_with_concurrency(Some("   ")).classify_concurrency_class(),
            ConcurrencyClassClassification::Unset
        );
    }
}
