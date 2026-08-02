//! Plugin manifest parsing.

use std::collections::HashMap;

use octos_plugin::{HardwareLifecycle, ToolDiscovery};
use serde::{Deserialize, Deserializer};

/// A plugin manifest (manifest.json).
#[derive(Debug, Deserialize)]
pub struct PluginManifest {
    /// Display and executable name. Legacy manifests use this as their
    /// identity when they do not declare an explicit `id`.
    #[serde(default)]
    pub name: String,
    /// Canonical plugin identity. This matches `octos-plugin` discovery,
    /// whose duplicate and root-precedence rules are keyed by manifest `id`.
    #[serde(default)]
    pub id: Option<String>,
    /// Plugin version.
    pub version: String,
    /// RFC-1 (issue #1290): the `content_type` discriminator this skill
    /// registers under for the unified `mofa_make({content_type, args})`
    /// dispatcher. When `Some`, the skill participates in the
    /// dispatcher's content_type enum, its `content_type_description` is
    /// surfaced verbatim to the LLM, and its primary "make target" tool
    /// (resolved via [`PluginManifest::make_target_tool_name`]) is hidden
    /// from the LLM-visible spec list — but stays callable internally so
    /// the dispatcher can forward to it.
    ///
    /// Manifests without this field behave exactly as they did before
    /// RFC-1: every tool surfaces by its own name. Skills outside the
    /// mofa-* content-generator family (e.g. mofa-fm voice management,
    /// weather, news) never set this field and remain top-level tools.
    #[serde(default)]
    pub make_type: Option<String>,
    /// RFC-1: human-readable blurb describing what this `content_type`
    /// generates. Surfaced verbatim inside the `mofa_make` tool
    /// description (concatenated into per-enum-value docs) so the LLM
    /// has enough context to pick the right `content_type`. Keep short
    /// (single sentence, <200 chars) — it shows up N times in the
    /// system-visible spec.
    #[serde(default)]
    pub content_type_description: Option<String>,
    /// RFC-1: explicit override for which tool inside this manifest is
    /// the dispatcher's "make target". When unset, resolution is:
    ///   1. tool named exactly `mofa_<make_type>` if present
    ///   2. otherwise the first tool with `spawn_only: true`
    ///   3. otherwise the first tool declared
    ///
    /// The override exists so a skill with several spawn_only tools
    /// (or with an unconventional naming scheme like `podcast_generate`)
    /// can pin the dispatcher's target without depending on declaration
    /// order. Sibling tools whose names are NOT this target stay visible
    /// to the LLM — e.g. `mofa_list_styles` next to `mofa_slides`.
    #[serde(default)]
    pub make_target_tool: Option<String>,
    /// Tools provided by this plugin.
    #[serde(default)]
    pub tools: Vec<PluginToolDef>,
    /// User-facing actions this skill allows UI clients to invoke directly.
    #[serde(default)]
    pub actions: Vec<SkillActionDef>,
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
    /// Optional hardware lifecycle (preflight / init / ready_check /
    /// shutdown / emergency_shutdown). Executed by the skill installer
    /// when present. Skills without hardware (most app skills) omit this.
    #[serde(default)]
    pub hardware_lifecycle: Option<HardwareLifecycle>,
    /// How this skill's tools are discovered. Defaults to `Static`
    /// (enumerated in `tools`); `Http` triggers dynamic discovery from
    /// a localhost bridge.
    #[serde(default)]
    pub tool_discovery: ToolDiscovery,
    /// Skill-level safety tier. Applies to all of this skill's tools UNLESS
    /// overridden by an entry in `tool_overrides` or the catalog's per-tool
    /// `safety_tier`. Defaults to `Observe` (read-only). Robots should set
    /// this to `safe_motion` at minimum.
    #[serde(default)]
    pub required_safety_tier: crate::permissions::SafetyTier,

    /// Per-verb safety tier overrides. Keyed by tool name. Lower priority
    /// than catalog tiers (the catalog is authoritative when present), higher
    /// priority than `required_safety_tier`.
    #[serde(default)]
    pub tool_overrides: HashMap<String, crate::permissions::SafetyTier>,

    /// Optional LLM-facing discovery summary.
    ///
    /// When present, `resolve_extras` renders a short 5-line "skill card"
    /// into the system prompt so the agent learns (1) the skill exists,
    /// (2) which tools it provides, and (3) where its directory lives.
    /// PR-F dropped the per-hint curation that PR-C/D originally shipped
    /// in favour of a one-paragraph generic preamble + `read_file`/`glob`/
    /// `list_dir` exploration over the skill directory (Claude Code's
    /// "you have the filesystem, go look" model).
    ///
    /// Legacy manifests that still declare `discovery.hints: [...]` parse
    /// cleanly because `SkillDiscovery` does not set
    /// `deny_unknown_fields` — the unknown field is silently dropped.
    #[serde(default)]
    pub discovery: Option<SkillDiscovery>,
}

impl PluginManifest {
    /// Canonical identity used for ownership, qualified action resolution,
    /// and cache keys. Legacy manifests retain their `name` identity.
    pub fn canonical_id(&self) -> &str {
        self.id
            .as_deref()
            .filter(|id| !id.trim().is_empty())
            .unwrap_or(&self.name)
    }

    /// Name used when resolving a plugin executable. Explicit names remain
    /// compatible with older packages that do not name their binary by `id`.
    pub fn executable_name(&self) -> &str {
        if self.name.trim().is_empty() {
            self.canonical_id()
        } else {
            &self.name
        }
    }

    /// Validate the manifest name before using it as the skill half of a
    /// qualified UI action identity. Existing plugins without actions are
    /// intentionally outside this validation path.
    pub fn validate_for_action_registration(&self) -> Result<(), ManifestValidationError> {
        if self.actions.is_empty() {
            return Ok(());
        }
        validate_action_text_field("id", self.canonical_id())?;
        if self.canonical_id().contains('/') {
            return Err(ManifestValidationError::InvalidActionField(
                "id",
                "must not contain '/'",
            ));
        }
        Ok(())
    }

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
    pub fn has_extras(&self) -> bool {
        !self.mcp_servers.is_empty()
            || !self.hooks.is_empty()
            || self.prompts.as_ref().is_some_and(|p| !p.include.is_empty())
            || self.discovery.is_some()
    }

    /// RFC-1: resolve the tool name the `mofa_make` dispatcher should
    /// forward to when this skill's `make_type` is selected.
    ///
    /// Resolution order:
    ///   1. `make_target_tool` if declared AND that tool exists in the
    ///      manifest (verified so a typo doesn't silently fall through).
    ///   2. Tool named `mofa_<make_type>` if present.
    ///   3. First tool with `spawn_only: true` (the canonical "generator").
    ///   4. First tool declared.
    ///
    /// Returns `None` when `make_type` is unset OR the manifest has zero
    /// tools (extras-only skills are not dispatcher targets).
    pub fn make_target_tool_name(&self) -> Option<&str> {
        let make_type = self.make_type.as_deref()?;
        if self.tools.is_empty() {
            return None;
        }
        // 1. Explicit override.
        if let Some(explicit) = self.make_target_tool.as_deref() {
            if self.tools.iter().any(|t| t.name == explicit) {
                return Some(explicit);
            }
            // Fall through to conventional lookup — operator typo
            // shouldn't break the dispatcher.
        }
        // 2. Conventional name.
        let conventional = format!("mofa_{make_type}");
        if let Some(t) = self.tools.iter().find(|t| t.name == conventional) {
            return Some(&t.name);
        }
        // 3. First spawn_only tool (canonical generator).
        if let Some(t) = self.tools.iter().find(|t| t.spawn_only) {
            return Some(&t.name);
        }
        // 4. First tool declared.
        self.tools.first().map(|t| t.name.as_str())
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

/// LLM-facing discovery summary declared by a skill manifest.
///
/// PR-F replaced the original per-hint curation (PR-C/D) with a single
/// `summary` line plus a generic "go read the skill_dir" preamble that
/// `resolve_extras` emits once per session. The renderer in `extras.rs`
/// turns this into a short 5-line skill card pushed into
/// `SkillExtras.prompt_fragments` (name / purpose / tools / skill_dir).
///
/// Legacy `hints: [...]` arrays still on disk parse cleanly because this
/// struct does NOT set `deny_unknown_fields`; serde silently drops the
/// field. A follow-up will scrub the dead arrays from the 7 mofa-skills
/// manifests.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, Default)]
pub struct SkillDiscovery {
    /// One-line description of what the skill does. Falls back to
    /// `"(no summary)"` in the rendered card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// A UI-callable action declared by a skill manifest.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct SkillActionDef {
    /// Stable action id within the skill, for example `source.import`.
    pub id: String,
    /// Human-readable label for menus and buttons.
    pub label: String,
    /// Optional short description for tooltips or secondary text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Semantic tags clients can filter on without coupling to a specific app.
    #[serde(default)]
    pub tags: Vec<String>,
    /// UI surfaces where this action is relevant, for example `studio.sources`.
    #[serde(default)]
    pub surfaces: Vec<String>,
    /// JSON Schema for the action-level input.
    #[serde(default = "default_schema")]
    pub input_schema: serde_json::Value,
    /// Optional UI hints. The host treats this as opaque metadata.
    #[serde(default)]
    pub ui_schema: serde_json::Value,
    /// Whether the action runs inline or as a supervised background job.
    #[serde(default)]
    pub execution: SkillActionExecution,
    /// Backend binding. UI clients cannot override this at invocation time.
    pub binding: SkillActionBinding,
}

/// How a UI-callable skill action is executed by the host.
#[derive(Debug, Clone, Copy, Deserialize, serde::Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SkillActionExecution {
    /// Invoke the bound backend operation before returning the AppUI response.
    #[default]
    Sync,
    /// Register a persisted supervised task and execute out of band.
    Background,
}

/// Backend binding for a UI-callable skill action.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SkillActionBinding {
    /// Invoke an existing registered tool.
    Tool {
        tool: String,
        #[serde(default)]
        input_mode: SkillActionInputMode,
        #[serde(default)]
        file_argument: Option<String>,
        #[serde(default)]
        file_materialization: SkillActionFileMaterialization,
    },
}

impl SkillActionBinding {
    pub fn tool_name(&self) -> Option<&str> {
        match self {
            Self::Tool { tool, .. } => Some(tool.as_str()),
        }
    }

    pub fn input_mode(&self) -> SkillActionInputMode {
        match self {
            Self::Tool { input_mode, .. } => *input_mode,
        }
    }

    pub fn file_argument(&self) -> Option<&str> {
        match self {
            Self::Tool { file_argument, .. } => file_argument.as_deref(),
        }
    }

    pub fn file_materialization(&self) -> SkillActionFileMaterialization {
        match self {
            Self::Tool {
                file_materialization,
                ..
            } => *file_materialization,
        }
    }
}

impl SkillActionDef {
    /// Validate fields needed to register an action under a stable qualified ID.
    pub fn validate_for_registration(&self) -> Result<(), ManifestValidationError> {
        validate_action_text_field("id", &self.id)?;
        if self.id.contains('/') {
            return Err(ManifestValidationError::InvalidActionField(
                "id",
                "must not contain '/'",
            ));
        }
        validate_action_text_field("label", &self.label)?;
        let tool = self.binding.tool_name().unwrap_or_default();
        validate_action_text_field("binding.tool", tool)?;
        Ok(())
    }
}

/// How action input is mapped to the bound tool call.
#[derive(Debug, Clone, Copy, Deserialize, serde::Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SkillActionInputMode {
    /// Forward the action arguments once.
    #[default]
    Single,
    /// Materialize `arguments.paths[]` and call the tool once per file.
    FileEach,
}

/// How `file_each` action paths are prepared before invoking the bound tool.
#[derive(Debug, Clone, Copy, Deserialize, serde::Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SkillActionFileMaterialization {
    /// Forward each string from `arguments.paths[]` unchanged.
    #[default]
    Raw,
    /// Copy upload references into `<workspace>/uploads/` and pass workspace-relative paths.
    WorkspaceRelative,
    /// Use the same upload handling as chat turn media.
    TurnMedia,
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
    /// Model contexts in which this tool may be advertised.
    /// Empty means the tool remains available in every context.
    #[serde(default)]
    pub contexts: Vec<String>,
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
    /// An action field cannot be used to form a stable trusted registration.
    InvalidActionField(&'static str, &'static str),
}

impl std::fmt::Display for ManifestValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidEnvName(name, reason) => write!(
                f,
                "manifest env allowlist entry {name:?} is invalid: {reason}"
            ),
            Self::InvalidActionField(field, reason) => {
                write!(f, "manifest action field {field:?} is invalid: {reason}")
            }
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

fn validate_action_text_field(
    field: &'static str,
    value: &str,
) -> Result<(), ManifestValidationError> {
    if value.trim().is_empty() {
        return Err(ManifestValidationError::InvalidActionField(
            field,
            "must not be empty",
        ));
    }
    if value != value.trim() || value.chars().any(char::is_control) {
        return Err(ManifestValidationError::InvalidActionField(
            field,
            "must not have surrounding whitespace or control characters",
        ));
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
        assert!(manifest.tools[0].contexts.is_empty());
    }

    #[test]
    fn should_parse_tool_model_contexts() {
        let json = r#"{
            "name": "notebook-plugin",
            "version": "1.0.0",
            "tools": [{
                "name": "source_search",
                "description": "Search notebook sources",
                "contexts": ["notebook"]
            }]
        }"#;
        let manifest: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(manifest.tools[0].contexts, vec!["notebook"]);
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
    fn test_id_based_manifest_can_omit_legacy_name() {
        let json = r#"{
            "id": "id-based-plugin",
            "version": "1.0.0",
            "tools": []
        }"#;
        let manifest = serde_json::from_str::<PluginManifest>(json).unwrap();
        assert_eq!(manifest.canonical_id(), "id-based-plugin");
        assert_eq!(manifest.executable_name(), "id-based-plugin");
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
            contexts: vec![],
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
    fn action_registration_validates_qualified_plugin_identity_names() {
        for name in [" bad-name ", "bad\nname", "bad/name"] {
            let manifest: PluginManifest = serde_json::from_value(serde_json::json!({
                "name": name,
                "version": "1.0",
                "actions": [{
                    "id": "identity.check",
                    "label": "Check identity",
                    "binding": {"type": "tool", "tool": "identity_tool"}
                }]
            }))
            .unwrap();
            assert!(manifest.validate_for_action_registration().is_err());
        }

        let no_actions: PluginManifest = serde_json::from_value(serde_json::json!({
            "name": " legacy/name ",
            "version": "1.0"
        }))
        .unwrap();
        assert!(no_actions.validate_for_action_registration().is_ok());
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
            contexts: vec![],
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

    // RFC-1 — make_type / dispatcher target resolution
    // ---------------------------------------------------------

    fn manifest_with_tools(make_type: Option<&str>, tools: Vec<PluginToolDef>) -> PluginManifest {
        PluginManifest {
            name: "test-skill".into(),
            id: None,
            version: "1.0".into(),
            make_type: make_type.map(str::to_string),
            content_type_description: None,
            make_target_tool: None,
            tools,
            actions: vec![],
            sha256: None,
            binaries: HashMap::new(),
            requires_network: false,
            timeout_secs: None,
            mcp_servers: vec![],
            hooks: vec![],
            prompts: None,
            hardware_lifecycle: None,
            tool_discovery: octos_plugin::ToolDiscovery::Static,
            required_safety_tier: crate::permissions::SafetyTier::default(),
            tool_overrides: HashMap::new(),
            discovery: None,
        }
    }

    fn tool(name: &str, spawn_only: bool) -> PluginToolDef {
        PluginToolDef {
            name: name.into(),
            description: "x".into(),
            input_schema: default_schema(),
            contexts: vec![],
            spawn_only,
            env: vec![],
            risk: None,
            spawn_only_message: None,
            concurrency_class: None,
        }
    }

    #[test]
    fn make_type_field_parses_from_json() {
        let json = r#"{
            "name": "mofa-slides",
            "version": "0.5.2",
            "make_type": "slides",
            "content_type_description": "PPTX decks",
            "tools": [{"name": "mofa_slides", "description": "d"}]
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.make_type.as_deref(), Some("slides"));
        assert_eq!(m.content_type_description.as_deref(), Some("PPTX decks"));
    }

    #[test]
    fn make_target_tool_resolution_prefers_explicit_override() {
        let mut m = manifest_with_tools(
            Some("podcast"),
            vec![
                tool("podcast_voices", false),
                tool("podcast_generate", true),
            ],
        );
        m.make_target_tool = Some("podcast_generate".into());
        assert_eq!(m.make_target_tool_name(), Some("podcast_generate"));
    }

    #[test]
    fn make_target_tool_resolution_finds_conventional_name() {
        let m = manifest_with_tools(
            Some("slides"),
            vec![tool("mofa_list_styles", false), tool("mofa_slides", true)],
        );
        assert_eq!(m.make_target_tool_name(), Some("mofa_slides"));
    }

    #[test]
    fn make_target_tool_resolution_falls_back_to_spawn_only() {
        // No mofa_podcast tool; the resolver falls back to the first
        // spawn_only=true tool (podcast_generate).
        let m = manifest_with_tools(
            Some("podcast"),
            vec![
                tool("podcast_voices", false),
                tool("podcast_generate", true),
            ],
        );
        assert_eq!(m.make_target_tool_name(), Some("podcast_generate"));
    }

    #[test]
    fn make_target_tool_resolution_falls_back_to_first_tool() {
        // No spawn_only tools at all; resolver returns the first tool.
        let m = manifest_with_tools(Some("video"), vec![tool("mofa_youtube", false)]);
        assert_eq!(m.make_target_tool_name(), Some("mofa_youtube"));
    }

    #[test]
    fn make_target_tool_resolution_returns_none_when_make_type_unset() {
        let m = manifest_with_tools(None, vec![tool("mofa_slides", true)]);
        assert!(m.make_target_tool_name().is_none());
    }

    #[test]
    fn make_target_tool_resolution_returns_none_for_empty_tools() {
        let m = manifest_with_tools(Some("slides"), vec![]);
        assert!(m.make_target_tool_name().is_none());
    }

    #[test]
    fn make_target_tool_resolution_recovers_from_invalid_override() {
        // Operator typo: `make_target_tool` points at a non-existent tool.
        // The resolver must fall through to conventional lookup instead
        // of returning `Some("nonexistent")` and creating a registry
        // miss at dispatch time.
        let mut m = manifest_with_tools(Some("cards"), vec![tool("mofa_cards", true)]);
        m.make_target_tool = Some("totally_wrong".into());
        assert_eq!(m.make_target_tool_name(), Some("mofa_cards"));
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

    #[test]
    fn hardware_lifecycle_optional_absent_means_none() {
        let json = r#"{
            "name": "test-skill",
            "version": "0.1.0"
        }"#;
        let manifest: PluginManifest = serde_json::from_str(json).unwrap();
        assert!(manifest.hardware_lifecycle.is_none());
    }

    #[test]
    fn hardware_lifecycle_parses_when_present() {
        let json = r#"{
            "name": "test-skill",
            "version": "0.1.0",
            "hardware_lifecycle": {
                "init": [
                    {"label": "start dataflow", "command": "echo start", "timeout_secs": 10}
                ],
                "shutdown": [
                    {"label": "stop dataflow", "command": "echo stop", "timeout_secs": 5}
                ]
            }
        }"#;
        let manifest: PluginManifest = serde_json::from_str(json).unwrap();
        let lc = manifest
            .hardware_lifecycle
            .expect("lifecycle should be present");
        assert_eq!(lc.init.len(), 1);
        assert_eq!(lc.init[0].label, "start dataflow");
        assert_eq!(lc.shutdown.len(), 1);
    }

    #[test]
    fn tool_discovery_defaults_to_static() {
        let json = r#"{
            "name": "test-skill",
            "version": "0.1.0"
        }"#;
        let manifest: PluginManifest = serde_json::from_str(json).unwrap();
        assert!(matches!(manifest.tool_discovery, ToolDiscovery::Static));
    }

    #[test]
    fn tool_discovery_http_parses() {
        let json = r#"{
            "name": "test-skill",
            "version": "0.1.0",
            "tool_discovery": {"type": "http", "base_url": "http://localhost:8765"}
        }"#;
        let manifest: PluginManifest = serde_json::from_str(json).unwrap();
        match manifest.tool_discovery {
            ToolDiscovery::Http { base_url } => {
                assert_eq!(base_url, "http://localhost:8765");
            }
            other => panic!("expected Http, got {other:?}"),
        }
    }

    #[test]
    fn required_safety_tier_defaults_to_observe() {
        let json = r#"{ "name": "s", "version": "0.1.0" }"#;
        let manifest: PluginManifest = serde_json::from_str(json).unwrap();
        assert!(matches!(
            manifest.required_safety_tier,
            crate::permissions::SafetyTier::Observe
        ));
    }

    #[test]
    fn required_safety_tier_parses_explicit_value() {
        let json = r#"{
            "name": "s", "version": "0.1.0",
            "required_safety_tier": "safe_motion"
        }"#;
        let manifest: PluginManifest = serde_json::from_str(json).unwrap();
        assert!(matches!(
            manifest.required_safety_tier,
            crate::permissions::SafetyTier::SafeMotion
        ));
    }

    #[test]
    fn tool_overrides_parses() {
        let json = r#"{
            "name": "s", "version": "0.1.0",
            "tool_overrides": {
                "robot.estop": "emergency_override",
                "vendor.x.y.motion.set_action": "safe_motion"
            }
        }"#;
        let manifest: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(manifest.tool_overrides.len(), 2);
        assert!(matches!(
            manifest.tool_overrides["robot.estop"],
            crate::permissions::SafetyTier::EmergencyOverride
        ));
        assert!(matches!(
            manifest.tool_overrides["vendor.x.y.motion.set_action"],
            crate::permissions::SafetyTier::SafeMotion
        ));
    }

    #[test]
    fn tool_overrides_absent_means_empty() {
        let json = r#"{ "name": "s", "version": "0.1.0" }"#;
        let manifest: PluginManifest = serde_json::from_str(json).unwrap();
        assert!(manifest.tool_overrides.is_empty());
    }

    // ------------------------------------------------------------------
    // SKILL.md PR-F: discovery field is summary-only; hints are gone
    // ------------------------------------------------------------------

    /// PR-F GREEN: a manifest declaring `discovery: { summary: "..." }`
    /// (and nothing else) parses cleanly and exposes the summary on the
    /// `SkillDiscovery` value. This is the canonical post-PR-F shape.
    #[test]
    fn manifest_parses_discovery_with_only_summary() {
        let json = r#"{
            "name": "mofa-slides",
            "version": "0.7.0",
            "tools": [{"name": "t", "description": "d"}],
            "discovery": {
                "summary": "Generate AI presentation slides with full-bleed Gemini images."
            }
        }"#;
        let manifest: PluginManifest = serde_json::from_str(json).unwrap();
        let discovery = manifest.discovery.expect("discovery present");
        assert_eq!(
            discovery.summary.as_deref(),
            Some("Generate AI presentation slides with full-bleed Gemini images.")
        );
    }

    /// PR-F backwards-tolerance: 7 mofa-skills manifests still ship the
    /// dead `discovery.hints: [...]` arrays on disk. PR-F must not break
    /// them — `SkillDiscovery` does NOT set `deny_unknown_fields`, so
    /// serde silently drops the field. This test pins that contract so a
    /// future tightening cannot break the migrated mofa-skills.
    #[test]
    fn manifest_silently_ignores_legacy_hints_field() {
        let json = r#"{
            "name": "legacy-mofa",
            "version": "1.0.0",
            "tools": [{"name": "t", "description": "d"}],
            "discovery": {
                "summary": "Card with legacy hints array still on disk.",
                "hints": [
                    { "when": "user asks anything", "read": "SKILL.md" },
                    { "when": "picking a style", "list": "styles/*.toml" }
                ]
            }
        }"#;
        let manifest: PluginManifest =
            serde_json::from_str(json).expect("legacy hints field must parse without error");
        let discovery = manifest.discovery.expect("discovery present");
        assert_eq!(
            discovery.summary.as_deref(),
            Some("Card with legacy hints array still on disk.")
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
                "summary": "Card-only skill."
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
                "summary": "Some skill."
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
    fn manifest_parses_ui_actions_bound_to_tools() {
        let json = r#"{
            "name": "mofa-notebook-source",
            "version": "0.1.0",
            "tools": [{"name": "source_import", "description": "Import source"}],
            "actions": [{
                "id": "source.import",
                "label": "Add source",
                "description": "Import uploaded files as reusable sources.",
                "tags": ["source", "document"],
                "surfaces": ["studio.sources"],
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "paths": {
                            "type": "array",
                            "items": { "type": "string" }
                        }
                    },
                    "required": ["paths"]
                },
                "ui_schema": {
                    "accept": [".md", ".txt", ".csv", ".json", ".html"]
                },
                "binding": {
                    "type": "tool",
                    "tool": "source_import",
                    "input_mode": "file_each",
                    "file_argument": "path",
                    "file_materialization": "workspace_relative"
                }
            }]
        }"#;

        let manifest: PluginManifest = serde_json::from_str(json).unwrap();

        assert_eq!(manifest.actions.len(), 1);
        let action = &manifest.actions[0];
        assert_eq!(action.id, "source.import");
        assert_eq!(action.label, "Add source");
        assert_eq!(action.tags, vec!["source", "document"]);
        assert_eq!(action.surfaces, vec!["studio.sources"]);
        assert_eq!(
            action.input_schema["required"],
            serde_json::json!(["paths"])
        );
        assert_eq!(action.ui_schema["accept"][0], ".md");
        assert_eq!(action.binding.tool_name(), Some("source_import"));
        assert_eq!(action.binding.input_mode(), SkillActionInputMode::FileEach);
        assert_eq!(action.binding.file_argument(), Some("path"));
        assert_eq!(
            action.binding.file_materialization(),
            SkillActionFileMaterialization::WorkspaceRelative
        );
        assert_eq!(action.execution, SkillActionExecution::Sync);
    }

    #[test]
    fn contract_fixture_exports_source_and_background_generate_actions() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../e2e/fixtures/compat-test-skill/manifest.json");
        let contents = std::fs::read_to_string(&path).expect("read skill action fixture");
        let manifest: PluginManifest =
            serde_json::from_str(&contents).expect("parse skill action fixture");

        assert_eq!(manifest.canonical_id(), "compat-test-skill");
        assert_eq!(manifest.version, "1.0.0");
        let source = manifest
            .actions
            .iter()
            .find(|action| action.id == "source.import")
            .expect("source.import action");
        let generate = manifest
            .actions
            .iter()
            .find(|action| action.id == "reports.generate")
            .expect("reports.generate action");
        assert_eq!(source.execution, SkillActionExecution::Sync);
        assert_eq!(generate.execution, SkillActionExecution::Background);
        assert_eq!(source.binding.tool_name(), Some("summarize_text"));
        assert_eq!(generate.binding.tool_name(), Some("summarize_text"));
        manifest.validate_for_action_registration().unwrap();
        for action in &manifest.actions {
            action.validate_for_registration().unwrap();
        }
    }

    #[test]
    fn manifest_parses_background_skill_action_execution() {
        let json = r#"{
            "name": "mofa-notebook-source",
            "version": "0.1.0",
            "tools": [{"name": "source_import", "description": "Import source"}],
            "actions": [{
                "id": "source.import",
                "label": "Add source",
                "execution": "background",
                "binding": {
                    "type": "tool",
                    "tool": "source_import",
                    "input_mode": "file_each",
                    "file_argument": "path",
                    "file_materialization": "workspace_relative"
                }
            }]
        }"#;

        let manifest: PluginManifest = serde_json::from_str(json).unwrap();

        assert_eq!(
            manifest.actions[0].execution,
            SkillActionExecution::Background
        );
    }

    // ------------------------------------------------------------------
    // SKILL.md PR-E: legacy skill_md_auto_inject field removal
    // ------------------------------------------------------------------

    /// PR-E backwards compat: manifests that still declare
    /// `skill_md_auto_inject` (the PR-D1 opt-out flag) must continue to
    /// parse cleanly after the field is dropped from `PluginManifest`.
    /// Serde silently ignores unknown JSON fields because the struct does
    /// NOT set `deny_unknown_fields`; this test pins that contract so a
    /// future tightening cannot break the 7 already-migrated mofa-* skill
    /// manifests on disk.
    #[test]
    fn manifest_ignores_legacy_skill_md_auto_inject_field() {
        let json = r#"{
            "name": "migrated",
            "version": "1.0.0",
            "tools": [{"name": "t", "description": "d"}],
            "skill_md_auto_inject": false
        }"#;
        let manifest: PluginManifest =
            serde_json::from_str(json).expect("legacy field must parse without error");
        assert_eq!(manifest.name, "migrated");
        assert_eq!(manifest.tools.len(), 1);
    }
}
