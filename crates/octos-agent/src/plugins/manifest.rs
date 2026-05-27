//! Plugin manifest parsing.

use std::collections::HashMap;

use serde::{Deserialize, Deserializer};

/// Maximum number of discovery hints a skill manifest may declare.
///
/// The hints render into the LLM system prompt as a per-skill "skill card";
/// growing the list without bound would silently push out other system
/// prompt content. 8 keeps each card to a ~10-line budget.
pub const MAX_DISCOVERY_HINTS: usize = 8;

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
    ///
    /// Empty-string values (`""`) are rejected at parse time: a manifest that
    /// goes to the trouble of declaring `sha256` must commit to an actual
    /// hex digest. Operators who want the legacy "unverified" path simply
    /// omit the field — which deserializes to `None` and (under
    /// `plugins.require_signed = false`) still loads with a warning.
    #[serde(default, deserialize_with = "deserialize_non_empty_sha256")]
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
    /// Optional LLM-facing discovery hints.
    ///
    /// When present, `resolve_extras` renders a short "skill card" into the
    /// system prompt so the agent learns (1) the skill exists, (2) where
    /// its directory lives, and (3) which files to read or list first.
    /// This is the opt-in migration affordance for the SKILL.md rethink:
    /// plugins that add `discovery` get the card; plugins without it keep
    /// the legacy SKILL.md auto-inject only (see `extras.rs`).
    #[serde(default, deserialize_with = "deserialize_discovery")]
    pub discovery: Option<SkillDiscovery>,
    /// When false, this plugin opts out of having its SKILL.md auto-injected
    /// into the system prompt. Used in combination with `discovery` (PR-C) to
    /// migrate plugins from legacy auto-inject to on-demand discovery.
    ///
    /// Migration is intentionally three steps so each stage stays revertable:
    ///   1. Add `discovery` so both legacy auto-inject AND the new skill
    ///      card render (validate the card works in production).
    ///   2. Set `skill_md_auto_inject: false` so only the card renders.
    ///   3. Trim SKILL.md body to <=80 lines now that the LLM reads it on
    ///      demand instead of having it shoved into every system prompt.
    ///
    /// Decoupling step 2 from step 1 via an explicit flag (rather than an
    /// implicit "presence of discovery disables legacy") is what lets each
    /// step be independently revertable — see PR-D1.
    ///
    /// Default: true (legacy behavior preserved for unmigrated plugins).
    #[serde(default = "default_true")]
    pub skill_md_auto_inject: bool,
}

/// Serde default helper for `bool` fields whose absence should deserialize
/// to `true`. Locally scoped: `PluginManifest::skill_md_auto_inject` is the
/// only field in this module that needs it today, but adding more later
/// (e.g. a `legacy_card_inject` flag) should reuse this helper.
fn default_true() -> bool {
    true
}

impl PluginManifest {
    /// Whether this manifest declares any extras (MCP servers, hooks, prompts,
    /// or discovery).
    ///
    /// Round-2 codex review BLOCKER 2: `discovery` MUST be counted here. The
    /// loader's strict-mode paths use `has_extras()` both to reject
    /// extras-only manifests (so the operator splits executable + extras into
    /// two skills they can hash independently) and to warn when dropping
    /// extras under `plugins.require_signed`. Omitting `discovery` from this
    /// check meant a discovery-only manifest became a silent no-op under
    /// strict mode, and a signed tool plugin with discovery had its skill
    /// card dropped without any warning.
    ///
    /// PR-D1 note: `skill_md_auto_inject` is intentionally NOT counted as an
    /// extra. The semantics are inverted relative to the other fields —
    /// setting it to `false` *removes* an injected fragment rather than
    /// adding one — so it does not affect whether the manifest carries
    /// surfaces beyond its tool list. The loader's strict-mode reasoning
    /// about extras-only manifests therefore continues to apply unchanged.
    pub fn has_extras(&self) -> bool {
        !self.mcp_servers.is_empty()
            || !self.hooks.is_empty()
            || self.prompts.as_ref().is_some_and(|p| !p.include.is_empty())
            || self.discovery.is_some()
    }
}

/// Reject empty-string `sha256` at parse time so callers cannot pass the
/// integrity gate by declaring the field with no value. A missing field
/// still deserializes to `None` (the legacy "unverified" path); only an
/// explicit `""` is treated as a hard error.
fn deserialize_non_empty_sha256<'de, D>(d: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error;
    let maybe = Option::<String>::deserialize(d)?;
    match maybe {
        Some(s) if s.trim().is_empty() => Err(D::Error::custom(
            "manifest.sha256 must be a non-empty hex digest (omit the field for unsigned plugins)",
        )),
        other => Ok(other),
    }
}

/// LLM-facing discovery hints declared by a skill manifest.
///
/// The renderer in `extras.rs` turns this into a short skill card pushed
/// into `SkillExtras.prompt_fragments`. The card tells the agent the skill
/// exists, where its directory lives, and which paths to read or list to
/// learn more. See PR-C of the SKILL.md rethink.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, Default)]
pub struct SkillDiscovery {
    /// One-line description of what the skill does. Falls back to
    /// `"(no summary)"` in the rendered card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Up to `MAX_DISCOVERY_HINTS` pointers the LLM can follow on its
    /// own (via the already-allowlisted `read_file`/`glob`/`list_dir`
    /// tools from PR-A + PR-B).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hints: Vec<DiscoveryHint>,
}

/// A single discovery hint: when this trigger applies, the LLM should
/// either read a file or list a glob pattern under the skill directory.
///
/// At least one of `read` or `list` must be set; both is allowed (the
/// renderer joins them with " OR "). Paths are validated at parse time
/// to reject `..` traversal — discovery hints feed the system prompt,
/// so a hostile manifest could otherwise nudge the LLM to read arbitrary
/// files via the now-permissive skill-dir scope from PR-A.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct DiscoveryHint {
    /// Human-readable trigger condition, e.g.
    /// `"user asks for editable PPT"`.
    pub when: String,
    /// Path (or fragment-anchored path) to read. Relative to the skill
    /// directory. Must not contain `..`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read: Option<String>,
    /// Glob pattern to list. Relative to the skill directory. Must not
    /// contain `..`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list: Option<String>,
}

/// Validating deserializer for `PluginManifest::discovery`.
///
/// Enforces:
/// * At most `MAX_DISCOVERY_HINTS` hints.
/// * Neither `read` nor `list` may contain `..` (path traversal).
/// * Each hint must declare at least one of `read` or `list`.
///
/// A rejected manifest is a hard error at parse time so a malicious or
/// typo'd discovery block never reaches the LLM-facing renderer.
fn deserialize_discovery<'de, D>(d: D) -> Result<Option<SkillDiscovery>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error;
    let maybe = Option::<SkillDiscovery>::deserialize(d)?;
    let Some(disc) = maybe else {
        return Ok(None);
    };

    if disc.hints.len() > MAX_DISCOVERY_HINTS {
        return Err(D::Error::custom(format!(
            "manifest.discovery.hints declares {} entries; the maximum allowed is {} (MAX_DISCOVERY_HINTS)",
            disc.hints.len(),
            MAX_DISCOVERY_HINTS
        )));
    }

    for (idx, hint) in disc.hints.iter().enumerate() {
        if hint.read.is_none() && hint.list.is_none() {
            return Err(D::Error::custom(format!(
                "manifest.discovery.hints[{idx}] declares neither `read` nor `list`; at least one is required"
            )));
        }
        if hint.read.as_deref().is_some_and(hint_path_has_traversal) {
            return Err(D::Error::custom(format!(
                "manifest.discovery.hints[{idx}].read contains path traversal (..): {:?}",
                hint.read
            )));
        }
        if hint.list.as_deref().is_some_and(hint_path_has_traversal) {
            return Err(D::Error::custom(format!(
                "manifest.discovery.hints[{idx}].list contains path traversal (..): {:?}",
                hint.list
            )));
        }
    }

    Ok(Some(disc))
}

/// Return `true` if a discovery-hint path (or glob pattern) contains a
/// `..` traversal segment. Splits on both `/` and `\` to cover Windows
/// authors editing manifests on Unix and vice versa, then matches the
/// trimmed segment exactly so `..foo` (legitimate filename) survives.
fn hint_path_has_traversal(path: &str) -> bool {
    path.split(['/', '\\']).any(|seg| seg.trim() == "..")
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
    /// loader can warn without rejecting (the runtime resolver in
    /// `PluginTool::concurrency_class` fails-closed to Exclusive on
    /// Unknown — see issue #718 — so a typo still serialises execution
    /// even before the operator notices the warn log).
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
    /// diagnostic logging. Runtime behavior fails-closed to Exclusive
    /// (see issue #718 — matches MCP's `resolved_concurrency_class`).
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
                "name": "search",
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

    /// Section B: empty-string `sha256` is rejected at parse time. A
    /// manifest that goes to the trouble of declaring the field must
    /// commit to a real digest — operators who want unsigned plugins
    /// simply omit the key.
    #[test]
    fn manifest_rejects_empty_sha256_at_parse_time() {
        let json = r#"{
            "name": "ghost",
            "version": "1.0.0",
            "sha256": "",
            "tools": [{"name": "t", "description": "d"}]
        }"#;
        let err = serde_json::from_str::<PluginManifest>(json)
            .expect_err("empty sha256 must fail to parse");
        let msg = err.to_string();
        assert!(
            msg.contains("non-empty"),
            "error must explain that sha256 cannot be empty; got: {msg}"
        );
    }

    /// Section B: whitespace-only `sha256` is also rejected.
    #[test]
    fn manifest_rejects_whitespace_only_sha256() {
        let json = r#"{
            "name": "ghost",
            "version": "1.0.0",
            "sha256": "   ",
            "tools": [{"name": "t", "description": "d"}]
        }"#;
        let err = serde_json::from_str::<PluginManifest>(json)
            .expect_err("whitespace sha256 must fail to parse");
        assert!(err.to_string().contains("non-empty"));
    }

    /// Section B: an explicit `null` and a missing field both yield
    /// `sha256 = None` (the legacy unverified path).
    #[test]
    fn manifest_accepts_missing_or_null_sha256_as_unsigned() {
        let missing = r#"{
            "name": "ghost",
            "version": "1.0.0",
            "tools": [{"name": "t", "description": "d"}]
        }"#;
        let m1: PluginManifest = serde_json::from_str(missing).unwrap();
        assert!(m1.sha256.is_none());

        let null_value = r#"{
            "name": "ghost",
            "version": "1.0.0",
            "sha256": null,
            "tools": [{"name": "t", "description": "d"}]
        }"#;
        let m2: PluginManifest = serde_json::from_str(null_value).unwrap();
        assert!(m2.sha256.is_none());
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

    // ------------------------------------------------------------------
    // SKILL.md PR-C: discovery field
    // ------------------------------------------------------------------

    #[test]
    fn manifest_parses_with_discovery_field() {
        let json = r#"{
            "name": "mofa-slides",
            "version": "0.6.1",
            "tools": [{"name": "t", "description": "d"}],
            "discovery": {
                "summary": "Generate AI presentation slides with full-bleed Gemini images.",
                "hints": [
                    { "when": "user asks for editable PPT", "read": "SKILL.md#mode-2" },
                    { "when": "picking a style", "list": "styles/*.toml" },
                    { "when": "authoring custom styles", "read": "docs/custom-styles.md" }
                ]
            }
        }"#;
        let manifest: PluginManifest = serde_json::from_str(json).unwrap();
        let discovery = manifest.discovery.expect("discovery present");
        assert_eq!(
            discovery.summary.as_deref(),
            Some("Generate AI presentation slides with full-bleed Gemini images.")
        );
        assert_eq!(discovery.hints.len(), 3);
        assert_eq!(discovery.hints[0].when, "user asks for editable PPT");
        assert_eq!(discovery.hints[0].read.as_deref(), Some("SKILL.md#mode-2"));
        assert!(discovery.hints[0].list.is_none());
        assert_eq!(discovery.hints[1].when, "picking a style");
        assert!(discovery.hints[1].read.is_none());
        assert_eq!(discovery.hints[1].list.as_deref(), Some("styles/*.toml"));
        assert_eq!(discovery.hints[2].when, "authoring custom styles");
        assert_eq!(
            discovery.hints[2].read.as_deref(),
            Some("docs/custom-styles.md")
        );
    }

    #[test]
    fn manifest_parses_without_discovery_field() {
        let json = r#"{
            "name": "legacy-plugin",
            "version": "1.0.0",
            "tools": [{"name": "t", "description": "d"}]
        }"#;
        let manifest: PluginManifest = serde_json::from_str(json).unwrap();
        assert!(manifest.discovery.is_none());
    }

    #[test]
    fn manifest_rejects_too_many_hints() {
        // 9 hints — exceeds MAX_DISCOVERY_HINTS (8).
        let json = r#"{
            "name": "noisy",
            "version": "1.0.0",
            "tools": [{"name": "t", "description": "d"}],
            "discovery": {
                "summary": "Too many hints.",
                "hints": [
                    { "when": "a", "read": "a.md" },
                    { "when": "b", "read": "b.md" },
                    { "when": "c", "read": "c.md" },
                    { "when": "d", "read": "d.md" },
                    { "when": "e", "read": "e.md" },
                    { "when": "f", "read": "f.md" },
                    { "when": "g", "read": "g.md" },
                    { "when": "h", "read": "h.md" },
                    { "when": "i", "read": "i.md" }
                ]
            }
        }"#;
        let err =
            serde_json::from_str::<PluginManifest>(json).expect_err("9 hints must fail to parse");
        let msg = err.to_string();
        assert!(
            msg.contains("hints") && msg.contains("8"),
            "error must mention hint cap; got: {msg}"
        );
    }

    #[test]
    fn manifest_rejects_traversal_in_hint_read() {
        let json = r#"{
            "name": "evil",
            "version": "1.0.0",
            "tools": [{"name": "t", "description": "d"}],
            "discovery": {
                "hints": [
                    { "when": "exfil", "read": "../etc/passwd" }
                ]
            }
        }"#;
        let err = serde_json::from_str::<PluginManifest>(json)
            .expect_err("traversal in `read` must fail to parse");
        let msg = err.to_string();
        assert!(
            msg.contains("..") || msg.to_lowercase().contains("traversal"),
            "error must mention traversal; got: {msg}"
        );
    }

    #[test]
    fn manifest_rejects_traversal_in_hint_list() {
        let json = r#"{
            "name": "evil",
            "version": "1.0.0",
            "tools": [{"name": "t", "description": "d"}],
            "discovery": {
                "hints": [
                    { "when": "exfil", "list": "../../*" }
                ]
            }
        }"#;
        let err = serde_json::from_str::<PluginManifest>(json)
            .expect_err("traversal in `list` must fail to parse");
        let msg = err.to_string();
        assert!(
            msg.contains("..") || msg.to_lowercase().contains("traversal"),
            "error must mention traversal; got: {msg}"
        );
    }

    /// Round-2 codex BLOCKER 2 regression: `has_extras()` must report true
    /// for a manifest whose only extras-bearing field is `discovery`. The
    /// loader's strict-mode rejection path (`require_signed && tools.is_empty()
    /// && has_extras()`) and the warn-then-drop path both rely on this; a
    /// false return here means a discovery-only signed manifest becomes a
    /// silent no-op instead of either failing closed or being announced in
    /// logs.
    #[test]
    fn has_extras_returns_true_for_discovery_only_manifest() {
        let json = r#"{
            "name": "discovery-only",
            "version": "1.0.0",
            "tools": [],
            "discovery": {
                "summary": "Card-only skill.",
                "hints": [
                    { "when": "user asks anything", "read": "SKILL.md" }
                ]
            }
        }"#;
        let manifest: PluginManifest = serde_json::from_str(json).unwrap();
        assert!(manifest.mcp_servers.is_empty());
        assert!(manifest.hooks.is_empty());
        assert!(manifest.prompts.is_none());
        assert!(
            manifest.has_extras(),
            "discovery-only manifest must count as extras-bearing"
        );
    }

    /// Companion check: a manifest with discovery alongside a tool also
    /// reports `has_extras() == true`, so the loader's "drop extras under
    /// require_signed" warn path fires (otherwise the skill card silently
    /// disappears for signed tool plugins).
    #[test]
    fn has_extras_returns_true_when_discovery_paired_with_tool() {
        let json = r#"{
            "name": "tool-with-discovery",
            "version": "1.0.0",
            "tools": [{"name": "t", "description": "d"}],
            "discovery": {
                "hints": [
                    { "when": "user asks anything", "read": "SKILL.md" }
                ]
            }
        }"#;
        let manifest: PluginManifest = serde_json::from_str(json).unwrap();
        assert!(manifest.has_extras());
    }

    /// Negative control: a manifest with no MCP, no hooks, no prompts, and
    /// no discovery still reports `has_extras() == false` so the loader's
    /// `tools.is_empty() && has_extras()` reject does not over-fire.
    #[test]
    fn has_extras_returns_false_for_bare_manifest() {
        let json = r#"{
            "name": "bare",
            "version": "1.0.0",
            "tools": [{"name": "t", "description": "d"}]
        }"#;
        let manifest: PluginManifest = serde_json::from_str(json).unwrap();
        assert!(!manifest.has_extras());
    }

    #[test]
    fn manifest_rejects_hint_with_no_read_or_list() {
        let json = r#"{
            "name": "blank",
            "version": "1.0.0",
            "tools": [{"name": "t", "description": "d"}],
            "discovery": {
                "hints": [
                    { "when": "user wants thing" }
                ]
            }
        }"#;
        let err = serde_json::from_str::<PluginManifest>(json)
            .expect_err("hint with neither read nor list must fail to parse");
        let msg = err.to_string();
        assert!(
            msg.to_lowercase().contains("read") || msg.to_lowercase().contains("list"),
            "error must explain hint needs read or list; got: {msg}"
        );
    }

    // ------------------------------------------------------------------
    // SKILL.md PR-D1: skill_md_auto_inject opt-out flag
    // ------------------------------------------------------------------

    /// A manifest that omits the field must deserialize to `true` so
    /// unmigrated plugins keep current behavior (legacy SKILL.md
    /// auto-inject still runs for `spawn_only` tools).
    #[test]
    fn manifest_defaults_skill_md_auto_inject_to_true() {
        let json = r#"{
            "name": "legacy",
            "version": "1.0.0",
            "tools": [{"name": "t", "description": "d"}]
        }"#;
        let manifest: PluginManifest = serde_json::from_str(json).unwrap();
        assert!(
            manifest.skill_md_auto_inject,
            "missing field must default to true (preserves legacy auto-inject)"
        );
    }

    /// Explicit `false` opts the plugin out of the legacy SKILL.md
    /// auto-inject. This is the migration knob PR-D2 will set per skill
    /// after the new discovery card lands and soaks.
    #[test]
    fn manifest_parses_explicit_skill_md_auto_inject_false() {
        let json = r#"{
            "name": "migrated",
            "version": "1.0.0",
            "tools": [{"name": "t", "description": "d"}],
            "skill_md_auto_inject": false
        }"#;
        let manifest: PluginManifest = serde_json::from_str(json).unwrap();
        assert!(
            !manifest.skill_md_auto_inject,
            "explicit false must turn off the legacy auto-inject gate"
        );
    }

    /// Explicit `true` is equivalent to omitting the field but must still
    /// be parseable so operators can be loud about opting in during the
    /// migration window.
    #[test]
    fn manifest_parses_explicit_skill_md_auto_inject_true() {
        let json = r#"{
            "name": "explicit-legacy",
            "version": "1.0.0",
            "tools": [{"name": "t", "description": "d"}],
            "skill_md_auto_inject": true
        }"#;
        let manifest: PluginManifest = serde_json::from_str(json).unwrap();
        assert!(manifest.skill_md_auto_inject);
    }
}
