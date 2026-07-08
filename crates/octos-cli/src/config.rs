//! Configuration file support for octos CLI.

use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};

/// Current config version.
const CURRENT_CONFIG_VERSION: u32 = 1;

/// Deployment mode determines how octos serve behaves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DeploymentMode {
    /// Standalone install — no tunnel, dashboard at /admin/.
    #[default]
    Local,
    /// Connected to a cloud server via frpc tunnel.
    Tenant,
    /// VPS relay server with tenant management and landing page.
    Cloud,
}

/// LLM provider configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Config {
    /// Config version for migration.
    #[serde(default)]
    pub version: Option<u32>,

    /// LLM provider: "anthropic", "openai", or "gemini".
    #[serde(default)]
    pub provider: Option<String>,

    /// Model name.
    #[serde(default)]
    pub model: Option<String>,

    /// Custom base URL for the API endpoint.
    #[serde(default)]
    pub base_url: Option<String>,

    /// Environment variable name for API key (default: ANTHROPIC_API_KEY, OPENAI_API_KEY, or GEMINI_API_KEY).
    #[serde(default)]
    pub api_key_env: Option<String>,

    /// Profile-scoped environment values, including API keys persisted by the dashboard/AppUI.
    #[serde(default)]
    pub env_vars: std::collections::HashMap<String, String>,

    /// Override auto-detected model behavior hints for the OpenAI provider.
    /// Useful for custom/unknown models behind OpenAI-compatible proxies.
    #[serde(default)]
    pub model_hints: Option<octos_llm::openai::ModelHints>,

    /// API protocol type: "openai" (default) or "anthropic".
    /// When set to "anthropic", the Anthropic Messages API format is used
    /// regardless of the provider name (for Anthropic-compatible proxies).
    #[serde(default)]
    pub api_type: Option<String>,

    /// Admin auth token (for dashboard login). Also settable via --auth-token CLI arg
    /// or OCTOS_AUTH_TOKEN env var.
    #[serde(default)]
    pub auth_token: Option<String>,

    /// Gateway configuration (optional).
    #[serde(default)]
    pub gateway: Option<GatewayConfig>,

    /// MCP server configurations.
    #[serde(default)]
    pub mcp_servers: Vec<octos_agent::McpServerConfig>,

    /// Sandbox configuration.
    #[serde(default)]
    pub sandbox: octos_agent::SandboxConfig,

    /// Tool access policy (allow/deny lists with group and wildcard support).
    #[serde(default)]
    pub tool_policy: Option<octos_agent::ToolPolicy>,

    /// Per-provider tool policies. Key = model ID or provider name prefix.
    /// Example: `{"gemini": {"deny": ["diff_edit"]}}`.
    #[serde(default)]
    pub tool_policy_by_provider: std::collections::HashMap<String, octos_agent::ToolPolicy>,

    /// Embedding configuration for hybrid memory search.
    #[serde(default)]
    pub embedding: Option<EmbeddingConfig>,

    /// Memory subsystem configuration.
    #[serde(default)]
    pub memory: Option<MemoryConfig>,

    /// Fallback models for provider failover chain.
    /// When the primary provider fails with a retriable error, the next model is tried.
    #[serde(default)]
    pub fallback_models: Vec<FallbackModel>,

    /// Maximum agent iterations per message (overridden by --max-iterations).
    #[serde(default)]
    pub max_iterations: Option<u32>,

    /// Lifecycle hooks for agent events.
    #[serde(default)]
    pub hooks: Vec<octos_agent::HookConfig>,

    /// Human-approval rules for tool calls that require a human decision
    /// before executing (suspend-and-resume flow on gateway channels — see
    /// `docs/ROBRIX-PHASE4-APPROVAL-FLOW-ADR.md`).
    #[serde(default)]
    pub approval_policy: Option<ApprovalPolicyConfig>,

    /// Context-based tool tag filter. When set, only tools matching at least one
    /// tag are visible to the LLM. Example: `["code", "search"]`.
    #[serde(default)]
    pub context_filter: Vec<String>,

    /// Sub-providers available for subagent spawning via the spawn tool.
    /// Each entry registers a provider under a short key that the LLM can reference.
    #[serde(default)]
    pub sub_providers: Vec<SubProviderConfig>,

    /// Adaptive routing configuration for dynamic provider selection.
    /// When enabled, replaces static priority failover with metrics-driven routing.
    #[serde(default)]
    pub adaptive_routing: Option<AdaptiveRoutingConfig>,

    /// Email sending configuration for the send_email tool.
    #[serde(default)]
    pub email: Option<EmailConfig>,

    /// Voice (ASR/TTS) configuration. When set, enables auto-transcription of
    /// voice messages and auto-TTS replies for voice conversations.
    #[serde(default)]
    pub voice: Option<VoiceConfig>,

    /// Deployment mode: "local" (default), "tenant", or "cloud".
    ///
    /// - `local`:  Standalone install, no tunnel, dashboard at /admin/
    /// - `tenant`: Connected to a cloud server via frpc tunnel
    /// - `cloud`:  VPS relay server with tenant management and landing page at /
    #[serde(default)]
    pub mode: DeploymentMode,

    /// Tunnel domain for cloud-host or tenant tunnel setups (e.g. "octos-cloud.org").
    /// Also read from TUNNEL_DOMAIN env var.
    #[serde(default)]
    pub tunnel_domain: Option<String>,

    /// Public-facing base domain each mini serves profiles under
    /// (e.g. `"crew.ominix.io"`, `"bot.ominix.io"`, `"ocean.ominix.io"`).
    ///
    /// Used to compose CORS allowlist entries and surface preview URLs
    /// in the admin dashboard. When `None` the server defaults to
    /// `"crew.ominix.io"` for backward compatibility. Also read from
    /// `OCTOS_BASE_DOMAIN` env var, which takes precedence over the
    /// value in `config.json` when both are set.
    #[serde(default)]
    pub base_domain: Option<String>,

    /// frps server address for cloud/tenant mode (e.g. "163.192.33.32").
    /// Also read from FRPS_SERVER env var.
    #[serde(default)]
    pub frps_server: Option<String>,

    /// Enable the admin shell endpoint (POST /api/admin/shell).
    /// Default: false. Only enable for development/debugging.
    /// A leaked admin token with this enabled grants full server access.
    #[serde(default)]
    pub allow_admin_shell: bool,

    /// Dashboard user authentication configuration (email OTP).
    /// When set, enables multi-user login via email verification codes.
    #[cfg(feature = "api")]
    #[serde(default)]
    pub dashboard_auth: Option<crate::otp::DashboardAuthConfig>,

    /// Monitor configuration for watchdog auto-restart and alerts.
    #[cfg(feature = "api")]
    #[serde(default)]
    pub monitor: Option<MonitorConfig>,

    /// Credential pool configuration (M6.5, F-005). Named pool of API
    /// keys / OAuth tokens with persistent cooldowns and rotation
    /// strategies. Absent → no pool is opened; adapters fall back to
    /// single-credential behavior.
    #[serde(default)]
    pub credential_pool: Option<CredentialPoolConfig>,

    /// Content-classified smart routing configuration (M6.6, F-005).
    /// Absent or `enabled: false` → every turn is classified as Strong
    /// (preserves pre-M6.6 routing behavior).
    #[serde(default)]
    pub content_routing: Option<octos_llm::RoutingConfig>,

    /// AppUi (octos-app, octos-tui, etc.) session defaults applied by
    /// `octos serve`. Operators can anchor every AppUi session that
    /// does not advertise the `session.workspace_cwd.v1` capability to
    /// a chosen folder via `appui.default_session_cwd` — the Tier-2
    /// fallback consulted by the UI Protocol dispatcher when no
    /// client-supplied cwd is present and before
    /// `SessionRuntime::bootstrap`'s Tier-3 profile-default workspace
    /// root. Capability-gated client-sent cwds (Tier-1) still take
    /// precedence.
    #[serde(default)]
    pub appui: AppUiConfig,

    /// Plugin loader policy. When `plugins.require_signed = true`, plugins
    /// without a `manifest.sha256` declaration are rejected at load time
    /// (instead of the legacy "warn and proceed" path). Default: false
    /// (backward compatible). Production fleets should turn this on after
    /// ensuring every shipped skill declares `sha256` in `manifest.json`.
    #[serde(default)]
    pub plugins: PluginsConfig,
}

/// Plugin loader policy.
///
/// All fields default to backward-compatible values so existing configs
/// continue to load plugins exactly as they did before this struct was
/// introduced. Set `require_signed = true` to enforce strict signature
/// verification — plugins without `manifest.sha256` will be rejected at
/// load time and re-hash gates apply on every invocation.
///
/// # Bundled / first-party skills caveat
///
/// First-party skills shipped under `crates/app-skills/*/manifest.json` and
/// `crates/platform-skills/*/manifest.json` currently do NOT declare
/// `sha256`. Enabling `require_signed = true` on a clean install will
/// therefore drop those tools (deep-search, weather, send-email, voice,
/// etc.) until the manifests are populated with the binaries' digests as
/// part of the release process. Production deployments that depend on
/// first-party skills should defer enabling this flag until the bundled
/// manifests ship `sha256`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PluginsConfig {
    /// When `true`, plugins must declare a `sha256` in their `manifest.json`
    /// and that hash must match the bytes on disk both at load time and
    /// before every invocation (pre-spawn re-hash closes the load→exec
    /// TOCTOU window). When `false` (default), unsigned plugins still load
    /// with a warning to preserve backward compatibility.
    ///
    /// See the [`PluginsConfig`] struct docs for a note on bundled
    /// first-party skills that ship without `sha256` today.
    #[serde(default)]
    pub require_signed: bool,
}

/// AppUi session defaults applied by `octos serve`'s API agent.
///
/// All fields are optional; an empty `[appui]` section preserves the
/// historical behavior (no server-side default cwd, every session falls
/// through Tier-3 of the `session_tool_registry` chain unchanged).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AppUiConfig {
    /// Optional default workspace cwd for AppUi sessions. When set, every
    /// `session/open` call against this server falls back to this cwd
    /// (Tier-2 of `session_tool_registry`'s fallback chain) when the
    /// client does not advertise `session.workspace_cwd.v1` and send its
    /// own cwd. Capability-gated client-sent cwds (Tier-1) take precedence.
    ///
    /// Use absolute paths. Tilde (`~`) is not expanded — operators who
    /// prefer a home-relative path should resolve it before writing
    /// `config.json`.
    #[serde(default)]
    pub default_session_cwd: Option<PathBuf>,
}

/// Top-level credential-pool configuration for `chat` / `serve`. Mirrors
/// the per-profile shape in `crate::profiles::CredentialPoolConfig` so
/// operators who do not use the multi-profile setup can still enable the
/// M6.5 pool via the top-level config.json.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialPoolConfig {
    /// Optional override for the persistent state file. Defaults to
    /// `<data_dir>/credential_pool.redb` when absent.
    #[serde(default)]
    pub state_path: Option<String>,
    /// Pool name used in metrics labels (e.g. `"anthropic"`). Default:
    /// `"default"`.
    #[serde(default = "default_credential_pool_name")]
    pub name: String,
    /// Rotation strategy identifier: `"fill_first"`, `"round_robin"`,
    /// `"random"`, `"least_used"`. Defaults to `round_robin`.
    #[serde(default = "default_credential_pool_strategy")]
    pub strategy: String,
    /// Credential ids that belong to the pool. Paired at runtime with
    /// API keys from `env_vars`.
    #[serde(default)]
    pub credential_ids: Vec<String>,
    /// Default cooldown applied to 429 responses without an explicit
    /// `reset_at` hint. Milliseconds.
    #[serde(default)]
    pub default_cooldown_ms: Option<u64>,
}

fn default_credential_pool_name() -> String {
    "default".into()
}

fn default_credential_pool_strategy() -> String {
    "round_robin".into()
}

impl Default for CredentialPoolConfig {
    fn default() -> Self {
        Self {
            state_path: None,
            name: default_credential_pool_name(),
            strategy: default_credential_pool_strategy(),
            credential_ids: Vec::new(),
            default_cooldown_ms: None,
        }
    }
}

/// A fallback model for the provider failover chain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FallbackModel {
    /// Provider name (e.g. "openai", "gemini").
    pub provider: String,
    /// Model name.
    #[serde(default)]
    pub model: Option<String>,
    /// Custom base URL.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Override the API key env var for this fallback.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Override auto-detected model hints for this fallback.
    #[serde(default)]
    pub model_hints: Option<octos_llm::openai::ModelHints>,
    /// API protocol type: "openai" or "anthropic". Overrides provider default.
    #[serde(default)]
    pub api_type: Option<String>,
    /// Published output price in USD per million tokens (for cost-aware routing).
    #[serde(default)]
    pub cost_per_m: Option<f64>,
    /// Mark as strong model (reliable with 30+ tools, large payloads).
    /// Used by slides sessions to filter failover candidates.
    /// Defaults to true for backward compat — set false for weak/proxy providers.
    #[serde(default = "default_true")]
    pub strong: bool,
}

pub fn default_true() -> bool {
    true
}

/// Default disposition for tools not matched by any approval rule.
/// v1 supports `allow` only (unmatched tools run without human approval).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalPolicyDefault {
    #[default]
    Allow,
}

/// Severity attached to approval requests (rendered by capable clients).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalPolicyRiskLevel {
    Normal,
    Critical,
}

/// What happens when an approval request expires unanswered.
/// v1 supports `notify` only (a notice is sent to the originating chat).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalPolicyTimeoutBehavior {
    Notify,
}

/// One human-approval rule: tool calls matching `tools` suspend the turn
/// until a user in `authorized_approvers` approves or denies, or the request
/// expires after `expires_in_secs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRuleConfig {
    /// Tool names this rule gates (exact match, e.g. `["shell", "write_file"]`).
    pub tools: Vec<String>,
    /// Must be `true` — present so a rule's intent is explicit in config.
    pub require_approval: bool,
    pub risk_level: ApprovalPolicyRiskLevel,
    /// Channel user IDs allowed to answer (e.g. `["@alice:example.org"]`).
    pub authorized_approvers: Vec<String>,
    /// Seconds until the pending request expires.
    pub expires_in_secs: u64,
    pub on_timeout: ApprovalPolicyTimeoutBehavior,
}

/// Config surface for the human-approval flow
/// (`docs/ROBRIX-PHASE4-APPROVAL-FLOW-ADR.md`). Converted to
/// [`octos_agent::HumanApprovalRules`] via [`Self::to_runtime_rules`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ApprovalPolicyConfig {
    #[serde(default)]
    pub default: ApprovalPolicyDefault,
    #[serde(default)]
    pub rules: Vec<ApprovalRuleConfig>,
}

impl ApprovalPolicyRiskLevel {
    pub fn to_runtime(self) -> octos_agent::ApprovalRiskLevel {
        match self {
            Self::Normal => octos_agent::ApprovalRiskLevel::Normal,
            Self::Critical => octos_agent::ApprovalRiskLevel::Critical,
        }
    }
}

impl ApprovalPolicyTimeoutBehavior {
    pub fn to_runtime(self) -> octos_agent::ApprovalTimeoutBehavior {
        match self {
            Self::Notify => octos_agent::ApprovalTimeoutBehavior::Notify,
        }
    }
}

impl ApprovalRuleConfig {
    pub fn to_runtime(&self) -> octos_agent::ApprovalRule {
        octos_agent::ApprovalRule {
            tools: self.tools.clone(),
            risk_level: self.risk_level.to_runtime(),
            authorized_approvers: self.authorized_approvers.clone(),
            expires_in_secs: self.expires_in_secs,
            on_timeout: self.on_timeout.to_runtime(),
        }
    }
}

impl ApprovalPolicyConfig {
    /// Validate every rule: non-empty `tools`, `require_approval` true,
    /// non-empty `authorized_approvers`, positive `expires_in_secs`. Shared by
    /// the top-level config load and the per-profile bootstrap path so a bad
    /// rule fails fast in both instead of gating unexpectedly / creating
    /// unanswerable or instantly-expiring requests (review finding #4).
    pub fn validate(&self) -> Result<()> {
        for (idx, rule) in self.rules.iter().enumerate() {
            if rule.tools.is_empty() {
                eyre::bail!("approval_policy.rules[{idx}].tools must not be empty");
            }
            if !rule.require_approval {
                eyre::bail!("approval_policy.rules[{idx}].require_approval must be true");
            }
            if rule.authorized_approvers.is_empty() {
                eyre::bail!("approval_policy.rules[{idx}].authorized_approvers must not be empty");
            }
            if rule.expires_in_secs == 0 {
                eyre::bail!("approval_policy.rules[{idx}].expires_in_secs must be > 0");
            }
        }
        Ok(())
    }

    pub fn to_runtime_rules(&self) -> octos_agent::HumanApprovalRules {
        octos_agent::HumanApprovalRules::new(
            self.rules
                .iter()
                .map(ApprovalRuleConfig::to_runtime)
                .collect(),
        )
    }
}

/// A sub-provider available for subagent spawning via the spawn tool.
/// The LLM sees these as selectable model options with cost/capability metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubProviderConfig {
    /// Short key used to reference this provider (e.g. "cheap", "strong").
    pub key: String,
    /// Provider name (e.g. "openai", "anthropic", "gemini").
    pub provider: String,
    /// Model name (e.g. "gpt-4o-mini").
    #[serde(default)]
    pub model: Option<String>,
    /// Environment variable name holding the API key for this sub-provider.
    /// If not set, falls back to the default for the provider (e.g. OPENAI_API_KEY).
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Custom base URL for this sub-provider.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Human-readable description of when/why to use this model.
    /// Shown to the LLM in the spawn tool schema.
    #[serde(default)]
    pub description: Option<String>,
    /// Default context window (tokens) applied when this sub-provider is selected.
    /// If set, sub-agents using this provider get this context budget automatically
    /// (unless the LLM explicitly overrides it). This controls how aggressively the
    /// sub-agent trims conversation history during its tool loop.
    #[serde(default)]
    pub default_context_window: Option<u32>,
    /// Maximum output tokens per LLM call for this model.
    /// If not set, auto-detected from the model name. Set explicitly when the
    /// auto-detection is wrong or for custom/local models.
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    /// API protocol type: "openai" or "anthropic". Overrides provider default.
    #[serde(default)]
    pub api_type: Option<String>,
}

/// Embedding provider configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingConfig {
    /// Provider name (currently only "openai").
    #[serde(default = "default_embedding_provider")]
    pub provider: String,

    /// Environment variable name for the API key (overrides provider default).
    #[serde(default)]
    pub api_key_env: Option<String>,

    /// Custom base URL for the embedding API.
    #[serde(default)]
    pub base_url: Option<String>,
}

fn default_embedding_provider() -> String {
    "openai".to_string()
}

/// Memory subsystem configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MemoryConfig {
    /// Token budget for the memory block injected into the system prompt
    /// (long-term memory + daily notes + bank summary combined). Defaults to
    /// [`octos_memory::DEFAULT_MAX_INJECT_TOKENS`]. The budget is spent in
    /// priority order (MEMORY.md, today's notes, bank abstracts, older daily
    /// notes) and omissions are disclosed to the model with a marker.
    #[serde(default)]
    pub max_inject_tokens: Option<usize>,

    /// Automatic memory refreshing (capture + consolidation pipeline).
    #[serde(default)]
    pub refresh: Option<MemoryRefreshConfig>,
}

/// Automatic memory-refresh settings. Default OFF: when disabled there is
/// no `memory_note` tool, no capture policy in the prompt, and no
/// per-turn memory re-read — behavior is identical to before the feature
/// existed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MemoryRefreshConfig {
    /// Master switch for the capture layer + read-side refresh.
    #[serde(default)]
    pub enabled: bool,
}

impl MemoryConfig {
    /// Effective injection budget, applying the default when unset.
    pub fn effective_max_inject_tokens(config: Option<&MemoryConfig>) -> usize {
        config
            .and_then(|m| m.max_inject_tokens)
            .unwrap_or(octos_memory::DEFAULT_MAX_INJECT_TOKENS)
    }

    /// Whether automatic memory refreshing (capture + read refresh) is on.
    pub fn refresh_enabled(config: Option<&MemoryConfig>) -> bool {
        config
            .and_then(|m| m.refresh.as_ref())
            .is_some_and(|r| r.enabled)
    }
}

/// Email sending configuration for the `send_email` tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmailConfig {
    /// Provider: "smtp" or "feishu" / "lark".
    pub provider: String,

    // -- SMTP fields --
    #[serde(default)]
    pub smtp_host: Option<String>,
    #[serde(default)]
    pub smtp_port: Option<u16>,
    #[serde(default)]
    pub username: Option<String>,
    /// Environment variable holding the SMTP password (legacy).
    #[serde(default)]
    pub password_env: Option<String>,
    /// SMTP password (literal value, preferred over password_env).
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub from_address: Option<String>,

    // -- Feishu/Lark fields --
    #[serde(default)]
    pub feishu_app_id: Option<String>,
    /// Environment variable holding the Feishu app secret (legacy).
    #[serde(default)]
    pub feishu_app_secret_env: Option<String>,
    /// Feishu app secret (literal value, preferred over feishu_app_secret_env).
    #[serde(default)]
    pub feishu_app_secret: Option<String>,
    #[serde(default)]
    pub feishu_from_address: Option<String>,
    /// "cn" (default) or "global".
    #[serde(default)]
    pub feishu_region: Option<String>,
}

/// Non-secret Volcano (cloud) TTS settings. The **persisted** secret (the API
/// token) is NEVER stored here — it lives in `env_vars["VOLC_TTS_TOKEN"]` so the
/// shared masking/restore machinery covers it. The `token` field below is a
/// runtime-only carrier (`#[serde(skip)]`): `ProfileRuntime::bootstrap` resolves
/// the token from the profile's `env_vars` and stashes it here so the in-process
/// `serve` path (which, unlike the gateway worker, does not get `env_vars`
/// injected into `std::env`) can authenticate. It is never serialized and the
/// `Debug` impl redacts it. Missing non-secret fields fall back to engine
/// defaults at resolve time (see `voice_turn::resolve_volcano`).
#[derive(Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CloudTtsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub appid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Runtime-only resolved API token (from `env_vars["VOLC_TTS_TOKEN"]`).
    /// Never serialized; redacted in `Debug`.
    #[serde(skip)]
    pub token: Option<String>,
}

impl std::fmt::Debug for CloudTtsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudTtsConfig")
            .field("appid", &self.appid)
            .field("voice", &self.voice)
            .field("cluster", &self.cluster)
            .field("encoding", &self.encoding)
            .field("endpoint", &self.endpoint)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// Voice (ASR/TTS) configuration for auto-transcription and auto-synthesis.
/// The OminiX API URL is a platform-wide setting via OMINIX_API_URL env var
/// (default http://localhost:8080), NOT per-profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoiceConfig {
    /// Legacy field — ignored. OminiX URL is now platform-wide via OMINIX_API_URL env var.
    #[serde(default, skip_serializing)]
    pub api_url: Option<String>,
    /// Auto-transcribe voice messages at gateway level. Default: true.
    #[serde(default = "voice_default_true")]
    pub auto_asr: bool,
    /// Auto-synthesize voice replies for voice conversations. Default: true.
    #[serde(default = "voice_default_true")]
    pub auto_tts: bool,
    /// Default TTS voice preset. Default: "vivian".
    #[serde(default = "default_voice_preset")]
    pub default_voice: String,
    /// Default ASR language hint. Default: None (auto-detect).
    #[serde(default)]
    pub asr_language: Option<String>,
    /// Which TTS route to use for synthesized replies:
    /// - `"auto"` (default): cloud when a token is configured, else on-device.
    /// - `"local"`: force the on-device ominix-api engine.
    /// - `"cloud"`: force cloud Volcano (falls back to on-device when the token
    ///   is missing or the request fails).
    ///
    /// Legacy aliases accepted for back-compat: `"volcano"` → `cloud`;
    /// `"sovits"` / `"qwen3"` → `local`.
    ///
    /// Cloud credentials: the non-secret settings live in `cloud` (CloudTtsConfig);
    /// the token is read from `VOLC_TTS_TOKEN` (never stored in config).
    #[serde(default = "default_tts_provider")]
    pub tts_provider: String,
    /// Non-secret cloud (Volcano) TTS settings. `None` → resolve entirely from
    /// `VOLC_TTS_*` env (back-compat). Per-profile override via
    /// `ProfileConfig.tts_cloud`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud: Option<CloudTtsConfig>,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            api_url: None,
            auto_asr: true,
            auto_tts: true,
            default_voice: default_voice_preset(),
            asr_language: None,
            tts_provider: default_tts_provider(),
            cloud: None,
        }
    }
}

impl VoiceConfig {
    /// Apply a per-profile timbre choice: replaces `default_voice` when
    /// `override_voice` is a non-empty per-user selection, leaving the
    /// platform-level route/ASR settings untouched. Used at profile bootstrap
    /// so each tenant remembers their own reply voice on top of the shared
    /// serve-level voice config.
    pub fn with_default_voice_override(mut self, override_voice: Option<&str>) -> Self {
        if let Some(v) = override_voice {
            if !v.is_empty() {
                self.default_voice = v.to_string();
            }
        }
        self
    }

    /// Apply a per-profile TTS route override (`auto`/`local`/`cloud`). Empty /
    /// `None` leaves the serve-level route untouched.
    pub fn with_tts_provider_override(mut self, override_provider: Option<&str>) -> Self {
        if let Some(p) = override_provider {
            if !p.is_empty() {
                self.tts_provider = p.to_string();
            }
        }
        self
    }

    /// Apply a per-profile cloud-TTS settings override. `None` leaves the
    /// serve-level (or env-fallback) settings untouched.
    pub fn with_cloud_override(mut self, override_cloud: Option<&CloudTtsConfig>) -> Self {
        if let Some(c) = override_cloud {
            self.cloud = Some(c.clone());
        }
        self
    }

    /// Resolve the cloud-TTS API token from a profile's `env_vars`
    /// (`VOLC_TTS_TOKEN`, keychain-aware) and stash it on `cloud.token`.
    ///
    /// This bridges the gap that the in-process `serve` path does not get the
    /// profile's `env_vars` injected into `std::env` (only the gateway worker
    /// does), so `voice_turn::resolve_volcano` can read the token from the
    /// runtime config instead. No-op when there is no `cloud` config, when the
    /// token is already set, or when `env_vars` has no (resolvable) token.
    pub fn with_cloud_token_from_env(
        mut self,
        env_vars: &std::collections::HashMap<String, String>,
    ) -> Self {
        if let Some(cloud) = self.cloud.as_mut() {
            if cloud.token.as_deref().unwrap_or("").is_empty() {
                if let Some(raw) = env_vars.get("VOLC_TTS_TOKEN") {
                    if let Some(resolved) =
                        crate::auth::keychain::resolve_value("VOLC_TTS_TOKEN", raw)
                    {
                        if !resolved.is_empty() {
                            cloud.token = Some(resolved);
                        }
                    }
                }
            }
        }
        self
    }
}

fn voice_default_true() -> bool {
    true
}
fn default_voice_preset() -> String {
    "vivian".to_string()
}
fn default_tts_provider() -> String {
    "auto".to_string()
}

/// Adaptive routing mode (config-level, maps to `AdaptiveMode` at runtime).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AdaptiveRoutingMode {
    /// Static priority order, failover only on circuit-broken.
    #[default]
    Off,
    /// Hedged racing: fire to 2 providers, take winner, cancel loser.
    Hedge,
    /// Score-based lane changing: dynamically pick the best single provider.
    Lane,
}

impl From<AdaptiveRoutingMode> for octos_llm::AdaptiveMode {
    fn from(m: AdaptiveRoutingMode) -> Self {
        match m {
            AdaptiveRoutingMode::Off => Self::Off,
            AdaptiveRoutingMode::Hedge => Self::Hedge,
            AdaptiveRoutingMode::Lane => Self::Lane,
        }
    }
}

/// Adaptive routing configuration for dynamic LLM provider selection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdaptiveRoutingConfig {
    /// Enable adaptive routing. Default: false.
    #[serde(default)]
    pub enabled: bool,

    /// Latency threshold (ms) above which a soft penalty is applied. Default: 10000.
    #[serde(default = "default_latency_threshold_ms")]
    pub latency_threshold_ms: u64,

    /// Error rate (0..1) above which provider is deprioritized. Default: 0.3.
    #[serde(default = "default_error_rate_threshold")]
    pub error_rate_threshold: f64,

    /// Probability (0..1) of probing a non-primary provider. Default: 0.1.
    #[serde(default = "default_probe_probability")]
    pub probe_probability: f64,

    /// Minimum seconds between probes to the same provider. Default: 60.
    #[serde(default = "default_probe_interval_secs")]
    pub probe_interval_secs: u64,

    /// Consecutive failures before circuit breaker opens. Default: 3.
    #[serde(default = "default_failure_threshold")]
    pub failure_threshold: u32,

    /// Adaptive mode: "off" (default), "hedge" (race 2 providers, take winner),
    /// or "lane" (score-based single-provider selection). Mutually exclusive.
    /// The ResponsivenessObserver can auto-escalate to "hedge" on degradation.
    #[serde(default)]
    pub mode: AdaptiveRoutingMode,

    /// Enable quality-of-service ranking that factors in response quality
    /// (not just latency/errors) when scoring providers. Orthogonal to mode.
    /// Default: false.
    #[serde(default)]
    pub qos_ranking: bool,

    /// Scoring weight for latency (0..1). Default: 0.3.
    #[serde(default = "default_weight_latency")]
    pub weight_latency: f64,
    /// Scoring weight for error rate (0..1). Default: 0.3.
    #[serde(default = "default_weight_error_rate")]
    pub weight_error_rate: f64,
    /// Scoring weight for config priority order (0..1). Default: 0.2.
    #[serde(default = "default_weight_priority")]
    pub weight_priority: f64,
    /// Scoring weight for published token cost (0..1). Default: 0.2.
    #[serde(default = "default_weight_cost")]
    pub weight_cost: f64,

    /// Auto-escalation: when sustained latency degradation is observed on a
    /// session, the router auto-promotes `mode` to `Hedge` and restores it
    /// on recovery. Defaults to enabled with FA-11/12-matching thresholds
    /// (8s ceiling, 3-consecutive-slow trigger, 0.6 recovery factor).
    /// Operators that explicitly want to disable the latency feedback loop
    /// can set `auto_escalation.enabled = false`.
    #[serde(default)]
    pub auto_escalation: AutoEscalationConfigFile,
}

impl Default for AdaptiveRoutingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            latency_threshold_ms: default_latency_threshold_ms(),
            error_rate_threshold: default_error_rate_threshold(),
            probe_probability: default_probe_probability(),
            probe_interval_secs: default_probe_interval_secs(),
            failure_threshold: default_failure_threshold(),
            mode: AdaptiveRoutingMode::Off,
            qos_ranking: false,
            weight_latency: default_weight_latency(),
            weight_error_rate: default_weight_error_rate(),
            weight_priority: default_weight_priority(),
            weight_cost: default_weight_cost(),
            auto_escalation: AutoEscalationConfigFile::default(),
        }
    }
}

/// Per-config auto-escalation tunables. Mirrors `octos_llm::AutoEscalationConfig`
/// but uses serde defaults so a missing `auto_escalation` block in
/// `config.json` resolves to the recommended values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutoEscalationConfigFile {
    #[serde(default = "default_auto_escalation_enabled")]
    pub enabled: bool,
    #[serde(default = "default_auto_escalation_window_size")]
    pub window_size: usize,
    #[serde(default = "default_auto_escalation_baseline_samples")]
    pub baseline_samples: usize,
    #[serde(default = "default_auto_escalation_degradation_threshold")]
    pub degradation_threshold: f64,
    #[serde(default = "default_auto_escalation_slow_trigger")]
    pub slow_trigger: u32,
    #[serde(default = "default_auto_escalation_latency_ceiling_ms")]
    pub latency_ceiling_ms: u64,
    #[serde(default = "default_auto_escalation_recovery_factor")]
    pub recovery_factor: f64,
}

impl Default for AutoEscalationConfigFile {
    fn default() -> Self {
        Self {
            enabled: default_auto_escalation_enabled(),
            window_size: default_auto_escalation_window_size(),
            baseline_samples: default_auto_escalation_baseline_samples(),
            degradation_threshold: default_auto_escalation_degradation_threshold(),
            slow_trigger: default_auto_escalation_slow_trigger(),
            latency_ceiling_ms: default_auto_escalation_latency_ceiling_ms(),
            recovery_factor: default_auto_escalation_recovery_factor(),
        }
    }
}

impl From<&AutoEscalationConfigFile> for octos_llm::AutoEscalationConfig {
    fn from(c: &AutoEscalationConfigFile) -> Self {
        Self {
            enabled: c.enabled,
            window_size: c.window_size,
            baseline_samples: c.baseline_samples,
            degradation_threshold: c.degradation_threshold,
            slow_trigger: c.slow_trigger,
            latency_ceiling_ms: c.latency_ceiling_ms,
            recovery_factor: c.recovery_factor,
        }
    }
}

fn default_auto_escalation_enabled() -> bool {
    true
}
fn default_auto_escalation_window_size() -> usize {
    5
}
fn default_auto_escalation_baseline_samples() -> usize {
    5
}
fn default_auto_escalation_degradation_threshold() -> f64 {
    3.0
}
fn default_auto_escalation_slow_trigger() -> u32 {
    3
}
fn default_auto_escalation_latency_ceiling_ms() -> u64 {
    8_000
}
fn default_auto_escalation_recovery_factor() -> f64 {
    0.6
}

impl From<&AdaptiveRoutingConfig> for octos_llm::AdaptiveConfig {
    fn from(c: &AdaptiveRoutingConfig) -> Self {
        Self {
            failure_threshold: c.failure_threshold,
            latency_threshold_ms: c.latency_threshold_ms,
            error_rate_threshold: c.error_rate_threshold,
            probe_probability: c.probe_probability,
            probe_interval_secs: c.probe_interval_secs,
            weight_latency: c.weight_latency,
            weight_error_rate: c.weight_error_rate,
            weight_priority: c.weight_priority,
            weight_cost: c.weight_cost,
            ..Default::default()
        }
    }
}

fn default_latency_threshold_ms() -> u64 {
    10_000
}
fn default_error_rate_threshold() -> f64 {
    0.3
}
fn default_probe_probability() -> f64 {
    0.1
}
fn default_probe_interval_secs() -> u64 {
    60
}
fn default_failure_threshold() -> u32 {
    3
}
fn default_weight_latency() -> f64 {
    0.3
}
fn default_weight_error_rate() -> f64 {
    0.3
}
fn default_weight_priority() -> f64 {
    0.2
}
fn default_weight_cost() -> f64 {
    0.2
}

/// Monitor configuration for watchdog auto-restart and alerts.
#[cfg(feature = "api")]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonitorConfig {
    /// Enable proactive alerts (default: true).
    #[serde(default = "monitor_default_true")]
    pub alerts_enabled: bool,
    /// Enable watchdog auto-restart (default: true).
    #[serde(default = "monitor_default_true")]
    pub watchdog_enabled: bool,
    /// Health check interval in seconds (default: 60).
    #[serde(default = "monitor_default_health_interval")]
    pub health_check_interval_secs: u64,
    /// Max auto-restart attempts before giving up (default: 3).
    #[serde(default = "monitor_default_max_restart")]
    pub max_restart_attempts: u32,
    /// Env var name for Telegram bot token used for alerts.
    #[serde(default)]
    pub telegram_token_env: Option<String>,
    /// Telegram chat IDs to send alerts to.
    #[serde(default)]
    pub telegram_alert_chat_ids: Vec<i64>,
    /// Env var name for Feishu app ID.
    #[serde(default)]
    pub feishu_app_id_env: Option<String>,
    /// Env var name for Feishu app secret.
    #[serde(default)]
    pub feishu_app_secret_env: Option<String>,
    /// Feishu user IDs to send alerts to.
    #[serde(default)]
    pub feishu_alert_user_ids: Vec<String>,
}

#[cfg(feature = "api")]
fn monitor_default_true() -> bool {
    true
}
#[cfg(feature = "api")]
fn monitor_default_health_interval() -> u64 {
    60
}
#[cfg(feature = "api")]
fn monitor_default_max_restart() -> u32 {
    3
}

impl Config {
    /// Directories to scan for plugins and skill packages with tools.
    ///
    /// Scans deployment-scoped dirs under `project_dir` (typically `octos_home`)
    /// plus dirs added via `OCTOS_SKILLS_PATH`. The legacy HOME-rooted globals
    /// (`~/.octos/skills`, `~/.octos/plugins`) are NO LONGER scanned — installs
    /// are per-profile only under `<data_dir>/skills/`. The bundled platform
    /// skills (`<octos_home>/platform-skills/`, admin-only) are loaded explicitly
    /// in serve.rs.
    ///
    /// When this function detects that the legacy `~/.octos/skills` directory
    /// still exists on disk it emits a one-shot `tracing::warn!` so operators
    /// migrating from older deployments see a clear migration prompt.
    ///
    /// The `project_dir` is typically `octos_home` (for managed gateways) or
    /// `cwd/.octos` (for standalone `octos chat`). This is intentionally decoupled
    /// from the agent's working directory (`cwd`) to support per-profile file
    /// isolation where `cwd` is narrowed to the profile's data directory.
    pub fn plugin_dirs_from_project(project_dir: &Path) -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        let local_plugins = project_dir.join("plugins");
        if local_plugins.exists() {
            dirs.push(local_plugins);
        }
        let local_skills = project_dir.join("skills");
        if local_skills.exists() {
            dirs.push(local_skills);
        }
        // Layered skill dirs
        let bundled = project_dir.join(octos_agent::bootstrap::BUNDLED_APP_SKILLS_DIR);
        if bundled.exists() {
            dirs.push(bundled);
        }
        // Note: platform-skills/ (voice, etc.) are admin-only — loaded explicitly in serve.rs
        // Legacy HOME-rooted globals (`~/.octos/skills`, `~/.octos/plugins`) are
        // deprecated: all skill installs now live under `<data_dir>/skills/` for
        // per-profile isolation. We still warn ONCE per process if the directory
        // is present so operators migrating from older deployments notice it.
        warn_once_if_legacy_global_skills_exist();
        // Extra dirs from OCTOS_SKILLS_PATH env var (colon-separated)
        if let Ok(extra) = std::env::var("OCTOS_SKILLS_PATH") {
            for p in extra.split(':') {
                let p = p.trim();
                if !p.is_empty() {
                    let path = PathBuf::from(p);
                    if path.exists() {
                        dirs.push(path);
                    }
                }
            }
        }
        dirs.dedup();
        dirs
    }
}

/// Section B (codex review round-5 P1.2): OR-merge
/// `OCTOS_PLUGINS_REQUIRE_SIGNED` (set by `ProcessManager` when the parent
/// serve enabled strict signing) onto the loaded Config. Spawned gateway
/// processes pick up the policy via env, even when the profile JSON they
/// load omits the new `plugins` block.
pub(crate) fn merge_env_plugin_policy_pub(config: &mut Config) {
    merge_env_plugin_policy(config);
    merge_env_memory_policy(config);
}

/// Fill `memory.max_inject_tokens` from `OCTOS_MEMORY_MAX_INJECT_TOKENS`
/// (set by `ProcessManager` from the host config.json) when the loaded
/// config leaves it unset. Field-level merge: an explicit value in the
/// loaded config always wins; the env var only fills the gap, so spawned
/// gateways inherit the host budget even when their profile JSON omits it
/// (or serializes an empty `memory: {}` block).
fn merge_env_memory_policy(config: &mut Config) {
    if config
        .memory
        .as_ref()
        .and_then(|m| m.max_inject_tokens)
        .is_none()
    {
        if let Ok(v) = std::env::var("OCTOS_MEMORY_MAX_INJECT_TOKENS") {
            if let Ok(n) = v.trim().parse::<usize>() {
                config
                    .memory
                    .get_or_insert_with(Default::default)
                    .max_inject_tokens = Some(n);
            }
        }
    }
    // Same field-level rule for the refresh switch: the env only fills the
    // gap when the loaded config says nothing about `memory.refresh`.
    if config
        .memory
        .as_ref()
        .and_then(|m| m.refresh.as_ref())
        .is_none()
    {
        if let Ok(v) = std::env::var("OCTOS_MEMORY_REFRESH_ENABLED") {
            let on = matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            );
            if on {
                config.memory.get_or_insert_with(Default::default).refresh =
                    Some(MemoryRefreshConfig { enabled: true });
            }
        }
    }
}

fn merge_env_plugin_policy(config: &mut Config) {
    if let Ok(v) = std::env::var("OCTOS_PLUGINS_REQUIRE_SIGNED") {
        let on = matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        );
        if on {
            config.plugins.require_signed = true;
        }
    }
}

/// One-shot warning when `~/.octos/skills` still exists on disk after we
/// stopped scanning it. Emitted at most once per process so operators see
/// a single migration hint rather than spamming every profile bootstrap.
fn warn_once_if_legacy_global_skills_exist() {
    use std::sync::Once;
    static WARN_ONCE: Once = Once::new();
    WARN_ONCE.call_once(|| {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        let legacy_skills = home.join(".octos").join("skills");
        let legacy_plugins = home.join(".octos").join("plugins");
        for legacy in [&legacy_skills, &legacy_plugins] {
            if legacy.exists() {
                tracing::warn!(
                    path = %legacy.display(),
                    "legacy global skill directory is no longer scanned; \
                     migrate contents into your profile's `<data_dir>/skills/` \
                     (e.g. `~/.octos/profiles/<id>/data/skills/`) — installs \
                     are per-profile only"
                );
            }
        }
    });
}

/// Message queue mode for handling messages arriving during active agent runs.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum QueueMode {
    /// Process queued messages one at a time (FIFO).
    Followup,
    /// Concatenate queued messages from the same session into one before processing.
    #[default]
    Collect,
    /// Keep only the latest message, discard older queued messages.
    Steer,
    /// Cancel the current run and process the new message immediately.
    Interrupt,
    /// If the current LLM call exceeds the patience threshold and a new message
    /// arrives, spawn a full agent task for the new message concurrently.
    /// Both results are delivered to the user.
    Speculative,
}

/// Gateway mode configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GatewayConfig {
    /// Channels to enable.
    #[serde(default)]
    pub channels: Vec<ChannelEntry>,

    /// Maximum conversation history messages to include.
    #[serde(default = "default_max_history")]
    pub max_history: usize,

    /// Custom system prompt for gateway mode.
    #[serde(default)]
    pub system_prompt: Option<String>,

    /// Message queue mode: "followup" (default) or "collect".
    #[serde(default)]
    pub queue_mode: QueueMode,

    /// Maximum sessions to keep in memory (LRU eviction). Default: 1000.
    #[serde(default = "default_max_sessions")]
    pub max_sessions: usize,

    /// Maximum concurrent session processing. Default: 10.
    #[serde(default = "default_max_concurrent_sessions")]
    pub max_concurrent_sessions: usize,

    /// Per-action timeout in seconds for the browser tool. Default: 300 (5 minutes).
    /// If a single browser action exceeds this, the session is killed and an error is returned.
    #[serde(default)]
    pub browser_timeout_secs: Option<u64>,

    /// LLM HTTP request timeout in seconds. Default: 120.
    #[serde(default)]
    pub llm_timeout_secs: Option<u64>,

    /// LLM HTTP connect timeout in seconds. Default: 30.
    #[serde(default)]
    pub llm_connect_timeout_secs: Option<u64>,

    /// Maximum seconds for all parallel tool calls to complete. Default: 300.
    #[serde(default)]
    pub tool_timeout_secs: Option<u64>,

    /// Maximum seconds for processing a single session message. Default: 600.
    #[serde(default)]
    pub session_timeout_secs: Option<u64>,

    /// Default max output tokens per LLM call. When set, overrides the built-in
    /// default from model_limits.json. Pipeline nodes can further override per-node.
    #[serde(default)]
    pub max_output_tokens: Option<u32>,

    /// Reasoning effort for thinking models (`low`|`medium`|`high`). Applied to
    /// every turn; only models that declare a reasoning style receive it
    /// (DeepSeek V4 gets `reasoning_effort` + `thinking`, OpenAI reasoning models
    /// and Grok get `reasoning_effort`), so non-thinking models silently ignore it.
    #[serde(default)]
    pub reasoning_effort: Option<octos_llm::ReasoningEffort>,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            channels: vec![ChannelEntry {
                channel_type: "cli".into(),
                allowed_senders: vec![],
                settings: serde_json::json!({}),
            }],
            max_history: default_max_history(),
            system_prompt: None,
            queue_mode: QueueMode::default(),
            max_sessions: default_max_sessions(),
            max_concurrent_sessions: default_max_concurrent_sessions(),
            browser_timeout_secs: None,
            llm_timeout_secs: None,
            llm_connect_timeout_secs: None,
            tool_timeout_secs: None,
            session_timeout_secs: None,
            max_output_tokens: None,
            reasoning_effort: None,
        }
    }
}

fn default_max_sessions() -> usize {
    1000
}

fn default_max_concurrent_sessions() -> usize {
    10
}

/// A channel entry in gateway config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelEntry {
    /// Channel type: "cli", "telegram", "discord".
    #[serde(rename = "type")]
    pub channel_type: String,

    /// Allowed sender IDs (empty = allow all).
    #[serde(default)]
    pub allowed_senders: Vec<String>,

    /// Channel-specific settings.
    #[serde(default)]
    pub settings: serde_json::Value,
}

fn default_max_history() -> usize {
    50
}

/// Load `config.json` as a raw `serde_json::Value`, apply `mutate`, and
/// atomically write the result back. Preserves unknown fields that the
/// strongly-typed [`Config`] struct would otherwise silently drop.
///
/// Creates the parent directory and an empty JSON object if the file does
/// not exist yet. Writes to a sibling `*.tmp` file first, then renames.
pub fn write_mutation<F>(path: &Path, mutate: F) -> Result<()>
where
    F: FnOnce(&mut serde_json::Value) -> Result<()>,
{
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .wrap_err_with(|| format!("failed to create dir: {}", parent.display()))?;
    }
    let mut value: serde_json::Value = if path.exists() {
        let body = std::fs::read_to_string(path)
            .wrap_err_with(|| format!("failed to read {}", path.display()))?;
        serde_json::from_str(&body)
            .wrap_err_with(|| format!("failed to parse {}", path.display()))?
    } else {
        serde_json::Value::Object(serde_json::Map::new())
    };
    mutate(&mut value)?;
    let body = serde_json::to_string_pretty(&value)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &body).wrap_err_with(|| format!("failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .wrap_err_with(|| format!("failed to rename into {}", path.display()))?;
    Ok(())
}

impl Config {
    /// Path to the runtime config file under the resolved data dir.
    pub fn data_dir_config_path(data_dir: &Path) -> PathBuf {
        data_dir.join("config.json")
    }

    /// Load config from the current project plus the canonical config context.
    ///
    /// This is the preferred entrypoint: it threads the single
    /// [`ConfigContext`](crate::config_context::ConfigContext) so the precedence
    /// is identical at every call site.
    pub fn load_with_context(
        cwd: &Path,
        ctx: &crate::config_context::ConfigContext,
    ) -> Result<Self> {
        Self::load_with_context_path(cwd, ctx).map(|(config, _)| config)
    }

    /// Like [`Self::load_with_context`] but also returns the resolved config
    /// path when one exists.
    pub fn load_with_context_path(
        cwd: &Path,
        ctx: &crate::config_context::ConfigContext,
    ) -> Result<(Self, Option<PathBuf>)> {
        Self::load_resolved(cwd, &ctx.config_home, ctx.is_default)
    }

    /// Core loader. Precedence:
    /// 1. (only when `is_default`) project-local `cwd/.octos/config.json`
    /// 2. `config_home/config.json`
    /// 3. (only when `is_default`) legacy `~/.octos/config.json`
    /// 4. defaults (with `merge_env_plugin_policy`)
    ///
    /// Explicit / tenant contexts (`is_default == false`) read ONLY from
    /// `config_home`. They MUST NOT read the ambient project-local
    /// `cwd/.octos/config.json` either: a tenant/`serve` process whose `cwd`
    /// happens to be `$HOME` would otherwise pick up the host's
    /// `~/.octos/config.json` (and, in `serve`, expose it to admin writes via
    /// `AppState.config_path`). That isolation is the whole point of this
    /// resolver. The project-local convenience is reserved for default-context
    /// `octos chat`/`gateway` invocations.
    fn load_resolved(
        cwd: &Path,
        config_home: &Path,
        is_default: bool,
    ) -> Result<(Self, Option<PathBuf>)> {
        // 1. Project-local config — DEFAULT context only. In explicit/tenant
        //    contexts this is skipped so an ambient `cwd/.octos/config.json`
        //    (e.g. the host's `~/.octos`) can never leak in.
        if is_default {
            let local_config = cwd.join(".octos").join("config.json");
            if local_config.exists() {
                tracing::info!(path = %local_config.display(), "loading config (project-local)");
                return Ok((Self::from_file(&local_config)?, Some(local_config)));
            }
        }

        // 2. The resolved config_home (XDG for default, data_dir/OCTOS_CONFIG_DIR
        //    for explicit). Resolved exactly once by `resolve_config_context`.
        let home_config = config_home.join("config.json");
        if home_config.exists() {
            tracing::info!(path = %home_config.display(), "loading config (config home)");
            return Ok((Self::from_file(&home_config)?, Some(home_config)));
        }

        // 3. Legacy back-compat: only for default installs, and only when the
        //    legacy path differs from config_home (so we don't double-check the
        //    same file). Explicit/tenant contexts never reach here.
        if is_default {
            if let Some(home) = dirs::home_dir() {
                let legacy_config = home.join(".octos").join("config.json");
                if legacy_config != home_config && legacy_config.exists() {
                    tracing::info!(
                        path = %legacy_config.display(),
                        "loading config (legacy ~/.octos — consider running `octos init` to migrate)"
                    );
                    return Ok((Self::from_file(&legacy_config)?, Some(legacy_config)));
                }
            }
        }

        // 4. No config found, use defaults. Even on the no-file path, honour
        // `OCTOS_PLUGINS_REQUIRE_SIGNED` so spawned gateways without a
        // config.json still inherit the host's strict-signing policy.
        tracing::info!("no config.json found, using defaults");
        let mut config = Self::default();
        merge_env_plugin_policy(&mut config);
        merge_env_memory_policy(&mut config);
        Ok((config, None))
    }

    /// Load config from a specific file.
    pub fn from_file(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .wrap_err_with(|| format!("failed to read config file: {}", path.display()))?;

        // Parse as raw Value first for migration
        let mut value: serde_json::Value = serde_json::from_str(&content)
            .wrap_err_with(|| format!("failed to parse config file: {}", path.display()))?;

        let migrated = migrate_config(&mut value);

        let mut config: Self = serde_json::from_value(value)
            .wrap_err_with(|| format!("failed to deserialize config: {}", path.display()))?;

        // Expand environment variables in config values
        config.expand_env_vars();
        config.validate_approval_policy()?;

        // Section B (codex review round-5 P1.2): the host's
        // `plugins.require_signed` policy must reach spawned gateway
        // processes too. `ProcessManager` sets `OCTOS_PLUGINS_REQUIRE_SIGNED=1`
        // when the parent serve was launched with strict signing; we
        // OR-merge that into every Config so a profile JSON that omits
        // the new block still inherits the strict policy. The host memory
        // budget rides the same mechanism via
        // `OCTOS_MEMORY_MAX_INJECT_TOKENS`.
        merge_env_plugin_policy(&mut config);
        merge_env_memory_policy(&mut config);

        // Log if migration changed something (don't silently rewrite user's config)
        if migrated {
            tracing::info!(
                path = %path.display(),
                version = CURRENT_CONFIG_VERSION,
                "Config file needs migration to version {}. Run `octos init` to update.",
                CURRENT_CONFIG_VERSION
            );
        }

        Ok(config)
    }

    /// Validate the human-approval rule set at config-load time so a typo'd
    /// rule fails fast instead of silently never gating (or gating with an
    /// unanswerable request). Runs after `expand_env_vars` so `${VAR}`
    /// references in approver lists are validated post-expansion.
    fn validate_approval_policy(&self) -> Result<()> {
        match &self.approval_policy {
            Some(policy) => policy.validate(),
            None => Ok(()),
        }
    }

    /// Expand environment variables in config values.
    /// Supports ${VAR_NAME} syntax.
    fn expand_env_vars(&mut self) {
        if let Some(ref mut base_url) = self.base_url {
            *base_url = Self::expand_env_var(base_url);
        }
        if let Some(ref mut model) = self.model {
            *model = Self::expand_env_var(model);
        }
        if let Some(ref mut provider) = self.provider {
            *provider = Self::expand_env_var(provider);
        }
        // Approval rules: expand ${VAR} in authorized_approvers so a
        // deployment can reference `${MATRIX_APPROVER}` etc. Without this the
        // literal `${VAR}` would pass the non-empty validation check and then
        // never match a real Matrix user id (review finding #4).
        if let Some(ref mut policy) = self.approval_policy {
            for rule in &mut policy.rules {
                for approver in &mut rule.authorized_approvers {
                    *approver = Self::expand_env_var(approver);
                }
            }
        }
    }

    /// Expand ${VAR_NAME} patterns in a string.
    fn expand_env_var(s: &str) -> String {
        let mut result = s.to_string();
        let mut start = 0;

        while let Some(begin) = result[start..].find("${") {
            let begin = start + begin;
            if let Some(end) = result[begin..].find('}') {
                let end = begin + end;
                let var_name = &result[begin + 2..end];
                if let Ok(value) = std::env::var(var_name) {
                    result = format!("{}{}{}", &result[..begin], value, &result[end + 1..]);
                    start = begin + value.len();
                } else {
                    start = end + 1;
                }
            } else {
                break;
            }
        }
        result
    }

    /// Get the API key: auth store first, then environment variable.
    pub fn get_api_key(&self, provider: &str) -> Result<String> {
        // Resolve the env var name we expect to hold this provider's key, and
        // mark it as a secret FIRST — before any early return — so the
        // configured key var is stripped from the default subprocess
        // environment regardless of which resolution path (auth store /
        // env_vars / keychain / process env) actually wins below. This also
        // covers a custom `api_key_env` whose NAME does not look secret to the
        // heuristic, so it can't be `echo`'d from the shell tool. Registered
        // names are still allowlistable: a tool that declares the var in its
        // manifest `env` list may receive it (the sanctioned path for skills
        // that call LLMs). See `octos_agent::subprocess_env`.
        let env_var = self.api_key_env.clone().unwrap_or_else(|| {
            octos_llm::registry::lookup(provider)
                .and_then(|e| e.api_key_env)
                .map(String::from)
                .unwrap_or_else(|| format!("{}_API_KEY", provider.to_uppercase()))
        });
        octos_agent::register_secret_env_names([env_var.as_str()]);

        // Check auth store first. Auth is GLOBAL: it lives under the resolver's
        // `auth_home` (OCTOS_CONFIG_DIR if set, else the XDG default). This is
        // independent of `--data-dir`, so per-profile gateways keep the host's
        // shared `octos auth login` credentials. We resolve the context with no
        // cli_data_dir because auth_home never depends on it.
        let auth_home = crate::config_context::resolve_config_context(None).auth_home;
        if let Ok(store) = crate::auth::AuthStore::at(&auth_home) {
            if let Some(cred) = store.get(provider) {
                if !cred.is_expired() {
                    return Ok(cred.access_token.clone());
                }
            }
        }

        if let Some(value) = self.env_vars.get(&env_var).and_then(|value| {
            crate::auth::keychain::resolve_value(&env_var, value).filter(|value| !value.is_empty())
        }) {
            return Ok(value);
        }

        std::env::var(&env_var).wrap_err_with(|| {
            format!("{env_var} not set. Run `octos auth login -p {provider}` or set the env var")
        })
    }

    /// Validate the configuration, returning any warnings.
    #[allow(clippy::manual_map)]
    pub fn validate(&self) -> Vec<String> {
        let mut warnings = Vec::new();

        // Check provider is valid
        if let Some(ref provider) = self.provider {
            if provider != "custom" && octos_llm::registry::lookup(provider).is_none() {
                let valid = octos_llm::registry::all_names();
                warnings.push(format!(
                    "Unknown provider '{}'. Valid options: {}",
                    provider,
                    valid.join(", ")
                ));
            }
        }

        // Check model/provider mismatch
        if let (Some(provider), Some(model)) = (&self.provider, &self.model) {
            if !is_valid_model_for_provider(provider, model) {
                warnings.push(format!(
                    "Model '{}' may not be valid for provider '{}'. Check provider docs.",
                    model, provider
                ));
            }
        }

        // Check base_url format
        if let Some(ref url) = self.base_url {
            if !(url.starts_with("http://") || url.starts_with("https://")) || url.contains(' ') {
                warnings.push(format!("base_url '{}' is not a valid URL", url));
            }
        }

        // Check gateway config
        if let Some(ref gw) = self.gateway {
            const VALID_CHANNELS: &[&str] = &[
                "cli",
                "telegram",
                "discord",
                "dingtalk",
                "slack",
                "whatsapp",
                "email",
                "feishu",
                "twilio",
                "wecom",
                "wecom-bot",
                "qq-bot",
                "wechat",
            ];
            for ch in &gw.channels {
                if !VALID_CHANNELS.contains(&ch.channel_type.as_str()) {
                    warnings.push(format!(
                        "Unknown channel type '{}'. Valid: {}",
                        ch.channel_type,
                        VALID_CHANNELS.join(", ")
                    ));
                }
            }
            if gw.max_history == 0 || gw.max_history > 1000 {
                warnings.push(format!(
                    "max_history {} is out of range (1-1000)",
                    gw.max_history
                ));
            }
        }

        // Check API key is set
        let provider = match self.provider.as_deref() {
            Some(p) => p,
            None => {
                warnings.push(
                    "No provider configured. Run 'octos init' to set up your LLM provider."
                        .to_string(),
                );
                return warnings;
            }
        };
        if self.get_api_key(provider).is_err() {
            let env_var = self.api_key_env.clone().unwrap_or_else(|| {
                octos_llm::registry::lookup(provider)
                    .and_then(|e| e.api_key_env)
                    .map(String::from)
                    .unwrap_or_else(|| format!("{}_API_KEY", provider.to_uppercase()))
            });
            warnings.push(format!("{} environment variable not set", env_var));
        }

        warnings
    }
}

/// Migrate config to current version. Returns true if anything changed.
fn migrate_config(value: &mut serde_json::Value) -> bool {
    let current = value.get("version").and_then(|v| v.as_u64()).unwrap_or(0) as u32;

    if current >= CURRENT_CONFIG_VERSION {
        return false;
    }

    // Future migrations go here:
    // if current < 2 { ... }

    // Set version to current
    value["version"] = serde_json::json!(CURRENT_CONFIG_VERSION);
    true
}

/// Check if a model name looks reasonable for a given provider.
/// Not exhaustive -- warns on clear mismatches only.
fn is_valid_model_for_provider(provider: &str, model: &str) -> bool {
    let m = model.to_lowercase();
    match provider {
        "anthropic" => m.contains("claude"),
        "openai" => {
            m.contains("gpt") || m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4")
        }
        "gemini" | "google" => m.contains("gemini"),
        "deepseek" => m.contains("deepseek"),
        "moonshot" | "kimi" => m.contains("kimi") || m.contains("moonshot"),
        "dashscope" | "qwen" => m.contains("qwen"),
        "zhipu" | "glm" => m.contains("glm"),
        "zai" | "z.ai" => true, // Z.AI hosts multiple models (GLM, Claude, etc.)
        "minimax" => m.contains("minimax"),
        // These host many models, accept any
        "groq" | "nvidia" | "nim" | "ollama" | "vllm" | "openrouter" => true,
        _ => true,
    }
}

/// Detect LLM provider from model name when no explicit provider is set.
pub fn detect_provider(model: &str) -> Option<&'static str> {
    octos_llm::registry::detect_provider(model)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Crate-wide lock for EVERY test that pivots the global `HOME` /
    /// `OCTOS_HOME` / `OCTOS_CONFIG_DIR` env vars. These are process-global, so
    /// all such tests (here and in `config_context`) must serialize against the
    /// SAME mutex — per-module locks would let env-mutating tests race across
    /// modules (a flaky-failure source).
    use crate::config_context::TEST_ENV_LOCK as HOME_ENV_LOCK;

    #[test]
    fn write_mutation_creates_file_with_pretty_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        write_mutation(&path, |v| {
            let obj = v.as_object_mut().unwrap();
            obj.insert("mode".into(), serde_json::json!("tenant"));
            Ok(())
        })
        .unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"mode\": \"tenant\""));
    }

    #[test]
    fn write_mutation_preserves_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{"mode":"local","unknown_field":{"keep":"me"},"nested":[1,2,3]}"#,
        )
        .unwrap();
        write_mutation(&path, |v| {
            v.as_object_mut()
                .unwrap()
                .insert("mode".into(), serde_json::json!("cloud"));
            Ok(())
        })
        .unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["mode"], "cloud");
        assert_eq!(parsed["unknown_field"]["keep"], "me");
        assert_eq!(parsed["nested"], serde_json::json!([1, 2, 3]));
    }

    #[test]
    fn write_mutation_round_trip_through_nested_block() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        write_mutation(&path, |v| {
            let obj = v.as_object_mut().unwrap();
            let auth = obj
                .entry("dashboard_auth")
                .or_insert_with(|| serde_json::json!({}));
            let smtp = auth
                .as_object_mut()
                .unwrap()
                .entry("smtp")
                .or_insert_with(|| serde_json::json!({}));
            let smtp = smtp.as_object_mut().unwrap();
            smtp.insert("host".into(), serde_json::json!("smtp.example.com"));
            smtp.insert("port".into(), serde_json::json!(465));
            Ok(())
        })
        .unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["dashboard_auth"]["smtp"]["host"], "smtp.example.com");
        assert_eq!(parsed["dashboard_auth"]["smtp"]["port"], 465);
    }

    #[test]
    #[allow(unsafe_code)]
    fn test_expand_env_var() {
        // SAFETY: test-only, single-threaded
        unsafe {
            std::env::set_var("TEST_VAR", "hello");
        }
        assert_eq!(Config::expand_env_var("${TEST_VAR}"), "hello");
        assert_eq!(
            Config::expand_env_var("prefix_${TEST_VAR}_suffix"),
            "prefix_hello_suffix"
        );
        assert_eq!(Config::expand_env_var("no_var"), "no_var");
        assert_eq!(
            Config::expand_env_var("${UNDEFINED_VAR}"),
            "${UNDEFINED_VAR}"
        );
        // SAFETY: test-only, single-threaded
        unsafe {
            std::env::remove_var("TEST_VAR");
        }
    }

    #[test]
    fn test_gateway_config_deserialize() {
        let json = r#"{
            "provider": "anthropic",
            "model": "claude-sonnet-4-20250514",
            "gateway": {
                "channels": [{"type": "cli"}],
                "max_history": 30
            }
        }"#;
        let config: Config = serde_json::from_str(json).unwrap();
        let gw = config.gateway.unwrap();
        assert_eq!(gw.channels.len(), 1);
        assert_eq!(gw.channels[0].channel_type, "cli");
        assert_eq!(gw.max_history, 30);
        assert!(gw.system_prompt.is_none());
        // reasoning_effort is optional and defaults to None when omitted.
        assert!(gw.reasoning_effort.is_none());
    }

    #[test]
    fn test_gateway_reasoning_effort_parses() {
        let json = r#"{
            "channels": [{"type": "cli"}],
            "reasoning_effort": "high"
        }"#;
        let gw: GatewayConfig = serde_json::from_str(json).unwrap();
        assert_eq!(gw.reasoning_effort, Some(octos_llm::ReasoningEffort::High));
    }

    #[test]
    fn test_gateway_config_defaults() {
        let json = r#"{"provider": "anthropic"}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert!(config.gateway.is_none());
    }

    #[test]
    fn test_gateway_max_history_default() {
        let json = r#"{"channels": [{"type": "cli"}]}"#;
        let gw: GatewayConfig = serde_json::from_str(json).unwrap();
        assert_eq!(gw.max_history, 50);
    }

    #[test]
    fn test_detect_provider_claude() {
        assert_eq!(
            detect_provider("claude-sonnet-4-20250514"),
            Some("anthropic")
        );
        assert_eq!(detect_provider("claude-3-haiku"), Some("anthropic"));
    }

    #[test]
    fn test_detect_provider_openai() {
        assert_eq!(detect_provider("gpt-4o"), Some("openai"));
        assert_eq!(detect_provider("o1-mini"), Some("openai"));
        assert_eq!(detect_provider("o3-mini"), Some("openai"));
    }

    #[test]
    fn test_detect_provider_others() {
        assert_eq!(detect_provider("gemini-2.0-flash"), Some("gemini"));
        assert_eq!(detect_provider("deepseek-chat"), Some("deepseek"));
        assert_eq!(detect_provider("kimi-k2.5"), Some("moonshot"));
        assert_eq!(detect_provider("qwen-max"), Some("dashscope"));
        assert_eq!(detect_provider("glm-4-plus"), Some("zhipu"));
        assert_eq!(detect_provider("llama-3.3-70b"), Some("groq"));
    }

    #[test]
    fn test_detect_provider_unknown() {
        assert_eq!(detect_provider("some-custom-model"), None);
    }

    #[test]
    fn test_validate_unknown_provider() {
        let config = Config {
            provider: Some("invalid".to_string()),
            ..Default::default()
        };
        let warnings = config.validate();
        assert!(warnings.iter().any(|w| w.contains("Unknown provider")));
    }

    #[test]
    fn test_validate_allows_custom_provider_with_base_url() {
        let config = Config {
            provider: Some("custom".to_string()),
            model: Some("llama-3.1-70b-instruct".to_string()),
            base_url: Some("http://127.0.0.1:11434/v1".to_string()),
            api_type: Some("openai".to_string()),
            api_key_env: Some("CUSTOM_API_KEY".to_string()),
            ..Default::default()
        };
        let warnings = config.validate();
        assert!(
            !warnings.iter().any(|w| w.contains("Unknown provider")),
            "custom provider config should not warn as unknown: {warnings:?}"
        );
    }

    #[test]
    fn test_validate_model_mismatch() {
        let config = Config {
            provider: Some("anthropic".to_string()),
            model: Some("gpt-4o".to_string()),
            ..Default::default()
        };
        let warnings = config.validate();
        assert!(warnings.iter().any(|w| w.contains("may not be valid")));
    }

    #[test]
    fn test_validate_invalid_base_url() {
        let config = Config {
            base_url: Some("not a url".to_string()),
            ..Default::default()
        };
        let warnings = config.validate();
        assert!(warnings.iter().any(|w| w.contains("not a valid URL")));
    }

    #[test]
    fn test_validate_invalid_channel_type() {
        let config = Config {
            gateway: Some(GatewayConfig {
                channels: vec![ChannelEntry {
                    channel_type: "irc".to_string(),
                    allowed_senders: vec![],
                    settings: serde_json::json!({}),
                }],
                max_history: 50,
                ..Default::default()
            }),
            ..Default::default()
        };
        let warnings = config.validate();
        assert!(warnings.iter().any(|w| w.contains("Unknown channel type")));
    }

    #[test]
    fn test_load_uses_resolved_data_dir_config() {
        let cwd = tempfile::tempdir().unwrap();
        let data_dir = tempfile::tempdir().unwrap();
        let data_dir_config = data_dir.path().join("config.json");
        std::fs::write(
            &data_dir_config,
            r#"{"provider":"openai","model":"gpt-4o"}"#,
        )
        .unwrap();

        // Explicit context: config_home == data_dir, is_default == false.
        let (config, path) = Config::load_resolved(cwd.path(), data_dir.path(), false).unwrap();
        assert_eq!(config.provider.as_deref(), Some("openai"));
        assert_eq!(config.model.as_deref(), Some("gpt-4o"));
        assert_eq!(path.as_deref(), Some(data_dir_config.as_path()));
    }

    #[test]
    fn test_load_prefers_project_local_over_data_dir_config() {
        let cwd = tempfile::tempdir().unwrap();
        let data_dir = tempfile::tempdir().unwrap();
        let local_dir = cwd.path().join(".octos");
        std::fs::create_dir_all(&local_dir).unwrap();
        let local_config = local_dir.join("config.json");
        let data_dir_config = data_dir.path().join("config.json");

        std::fs::write(
            &local_config,
            r#"{"provider":"anthropic","model":"claude-sonnet-4-20250514"}"#,
        )
        .unwrap();
        std::fs::write(
            &data_dir_config,
            r#"{"provider":"openai","model":"gpt-4o"}"#,
        )
        .unwrap();

        // Project-local precedence is a DEFAULT-context convenience.
        let (config, path) = Config::load_resolved(cwd.path(), data_dir.path(), true).unwrap();
        assert_eq!(config.provider.as_deref(), Some("anthropic"));
        assert_eq!(path.as_deref(), Some(local_config.as_path()));
    }

    /// Explicit/tenant context MUST NOT read the ambient project-local
    /// `cwd/.octos/config.json` — only `config_home`. This closes the codex P1
    /// where a tenant `serve` with `cwd == $HOME` picked up the host's
    /// `~/.octos/config.json` and exposed it to admin writes.
    #[test]
    fn load_explicit_ignores_project_local_config() {
        let cwd = tempfile::tempdir().unwrap();
        let config_home = tempfile::tempdir().unwrap();
        let local_dir = cwd.path().join(".octos");
        std::fs::create_dir_all(&local_dir).unwrap();
        // Ambient project-local config that would leak if isolation broke.
        std::fs::write(
            local_dir.join("config.json"),
            r#"{"provider":"anthropic","auth_token":"HOST_SECRET"}"#,
        )
        .unwrap();

        // Explicit context (is_default == false), empty config_home.
        let (config, path) = Config::load_resolved(cwd.path(), config_home.path(), false).unwrap();
        assert!(
            config.provider.is_none(),
            "explicit context must NOT read cwd/.octos/config.json"
        );
        assert!(config.auth_token.is_none());
        assert!(path.is_none());
    }

    /// Gate 1: default install + XDG config present + an intact legacy
    /// ~/.octos/config.json → XDG wins (legacy is the lower-precedence
    /// fallback, not consulted when config_home has a file).
    #[test]
    fn load_default_prefers_xdg_config_home_over_legacy() {
        // config_home (XDG) holds a config; legacy is a *different* path that
        // also holds one. With is_default == true, config_home must win.
        let cwd = tempfile::tempdir().unwrap();
        let xdg = tempfile::tempdir().unwrap();
        let xdg_config = xdg.path().join("config.json");
        std::fs::write(&xdg_config, r#"{"provider":"openai"}"#).unwrap();

        let (config, path) = Config::load_resolved(cwd.path(), xdg.path(), true).unwrap();
        assert_eq!(config.provider.as_deref(), Some("openai"));
        assert_eq!(path.as_deref(), Some(xdg_config.as_path()));
    }

    /// Gate 2 (back-compat): default install where config_home (XDG) has NO
    /// config but the legacy ~/.octos/config.json does → legacy loads.
    #[test]
    #[allow(unsafe_code)]
    fn load_default_falls_back_to_legacy_home_octos() {
        let _g = HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let tmp = tempfile::tempdir().unwrap();
        let fake_home = tmp.path();
        let cwd = fake_home.join("work");
        std::fs::create_dir_all(&cwd).unwrap();

        // Legacy ~/.octos/config.json present; XDG (config_home) empty.
        let legacy = fake_home.join(".octos").join("config.json");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, r#"{"provider":"gemini"}"#).unwrap();

        let empty_config_home = fake_home.join("empty-xdg");

        let original_home = std::env::var_os("HOME");
        // SAFETY: single-threaded inside LOCK; restored below.
        unsafe { std::env::set_var("HOME", fake_home) };

        let result = Config::load_resolved(&cwd, &empty_config_home, true);

        match original_home {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }

        let (config, path) = result.unwrap();
        assert_eq!(config.provider.as_deref(), Some("gemini"));
        assert_eq!(path.as_deref(), Some(legacy.as_path()));
    }

    /// Gate 2 (defaults): default install, neither XDG nor legacy → defaults.
    #[test]
    #[allow(unsafe_code)]
    fn load_default_uses_defaults_when_no_config_anywhere() {
        let _g = HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let tmp = tempfile::tempdir().unwrap();
        let fake_home = tmp.path();
        let cwd = fake_home.join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let empty_config_home = fake_home.join("empty-xdg");

        let original_home = std::env::var_os("HOME");
        // SAFETY: single-threaded inside LOCK; restored below.
        unsafe { std::env::set_var("HOME", fake_home) };

        let result = Config::load_resolved(&cwd, &empty_config_home, true);

        match original_home {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }

        let (config, path) = result.unwrap();
        assert!(config.provider.is_none());
        assert!(path.is_none());
    }

    /// Gate 3 (tenant isolation): explicit context (is_default == false) with
    /// an empty config_home MUST NOT fall through to the host's legacy
    /// ~/.octos/config.json — it loads defaults instead.
    #[test]
    #[allow(unsafe_code)]
    fn load_explicit_never_reads_host_legacy_octos() {
        let _g = HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let tmp = tempfile::tempdir().unwrap();
        let fake_home = tmp.path();
        let cwd = fake_home.join("work");
        std::fs::create_dir_all(&cwd).unwrap();

        // Host legacy config is present and would leak if isolation broke.
        let legacy = fake_home.join(".octos").join("config.json");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, r#"{"provider":"anthropic","auth_token":"SECRET"}"#).unwrap();

        // Explicit tenant data dir, no config inside it.
        let tenant = fake_home.join("tenant-data");

        let original_home = std::env::var_os("HOME");
        // SAFETY: single-threaded inside LOCK; restored below.
        unsafe { std::env::set_var("HOME", fake_home) };

        let result = Config::load_resolved(&cwd, &tenant, false);

        match original_home {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }

        let (config, path) = result.unwrap();
        assert!(
            config.provider.is_none(),
            "explicit/tenant context must NOT read host ~/.octos/config.json"
        );
        assert!(config.auth_token.is_none());
        assert!(path.is_none());
    }

    /// Gate 7 (end-to-end, the load-bearing "no per-profile login regression"):
    /// `get_api_key` resolves the GLOBAL auth store (XDG `auth_home`), NOT a
    /// per-profile `data_dir/auth.json`. We seed a credential at the XDG
    /// location and prove the API-key lookup finds it. `OCTOS_CONFIG_DIR` must
    /// be unset for the default/global case, so this test serializes on the
    /// shared env lock and clears all three env vars.
    #[test]
    #[allow(unsafe_code)]
    fn get_api_key_reads_global_xdg_auth_store() {
        let _g = HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let tmp = tempfile::tempdir().unwrap();
        let fake_home = tmp.path();

        let original_home = std::env::var_os("HOME");
        let original_octos_home = std::env::var_os("OCTOS_HOME");
        let original_octos_config = std::env::var_os("OCTOS_CONFIG_DIR");
        // Must also clear XDG_CONFIG_HOME: auth_home derives from it, so an
        // ambient absolute value would write auth.json outside the temp HOME.
        let original_xdg = std::env::var_os("XDG_CONFIG_HOME");
        // SAFETY: serialized by HOME_ENV_LOCK; restored below.
        unsafe {
            std::env::set_var("HOME", fake_home);
            std::env::remove_var("OCTOS_HOME");
            std::env::remove_var("OCTOS_CONFIG_DIR");
            std::env::remove_var("XDG_CONFIG_HOME");
        }

        // Resolve the GLOBAL auth_home (XDG) and seed a credential there.
        let ctx = crate::config_context::resolve_config_context(None);
        let mut store = crate::auth::AuthStore::open(&ctx).unwrap();
        store
            .set(
                "anthropic",
                crate::auth::AuthCredential {
                    access_token: "global-xdg-token".to_string(),
                    refresh_token: None,
                    expires_at: None,
                    provider: "anthropic".to_string(),
                    auth_method: "paste_token".to_string(),
                },
            )
            .unwrap();

        // A default config — get_api_key should consult the GLOBAL auth store
        // and return the seeded token (it does NOT look at any data_dir).
        let config = Config::default();
        let key = config.get_api_key("anthropic");

        match original_home {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        match original_octos_home {
            Some(v) => unsafe { std::env::set_var("OCTOS_HOME", v) },
            None => unsafe { std::env::remove_var("OCTOS_HOME") },
        }
        match original_octos_config {
            Some(v) => unsafe { std::env::set_var("OCTOS_CONFIG_DIR", v) },
            None => unsafe { std::env::remove_var("OCTOS_CONFIG_DIR") },
        }
        match original_xdg {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }

        assert_eq!(
            key.unwrap(),
            "global-xdg-token",
            "get_api_key must read the GLOBAL XDG auth store (shared login)"
        );
    }

    #[test]
    fn test_embedding_config_deserialize() {
        let json = r#"{
            "provider": "anthropic",
            "embedding": {
                "provider": "openai",
                "base_url": "https://custom.api.com/v1"
            }
        }"#;
        let config: Config = serde_json::from_str(json).unwrap();
        let emb = config.embedding.unwrap();
        assert_eq!(emb.provider, "openai");
        assert_eq!(emb.base_url.unwrap(), "https://custom.api.com/v1");
        assert!(emb.api_key_env.is_none());
    }

    #[test]
    fn test_embedding_config_absent() {
        let json = r#"{"provider": "anthropic"}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert!(config.embedding.is_none());
    }

    #[test]
    fn test_tool_policy_by_provider_deserialize() {
        let json = r#"{
            "provider": "anthropic",
            "tool_policy_by_provider": {
                "gemini": {"deny": ["diff_edit"]},
                "claude-sonnet-4-20250514": {"allow": ["shell", "read_file"]}
            }
        }"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert_eq!(config.tool_policy_by_provider.len(), 2);
        assert!(config.tool_policy_by_provider.contains_key("gemini"));
        assert!(
            config
                .tool_policy_by_provider
                .contains_key("claude-sonnet-4-20250514")
        );
    }

    #[test]
    fn test_tool_policy_by_provider_absent() {
        let json = r#"{"provider": "anthropic"}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert!(config.tool_policy_by_provider.is_empty());
    }

    #[test]
    fn should_deserialize_base_domain_from_config_json() {
        let json = r#"{"base_domain": "ocean.ominix.io"}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert_eq!(config.base_domain.as_deref(), Some("ocean.ominix.io"));
    }

    /// Section A of the per-profile-skills migration: the legacy HOME-rooted
    /// globals (`~/.octos/skills`, `~/.octos/plugins`) MUST NOT appear in the
    /// scan list anymore. Installs live under `<data_dir>/skills/` for
    /// per-profile isolation; HOME-rooted globals are deprecated.
    ///
    /// This test pivots `HOME` to a temp dir so it works on CI hosts where
    /// the real `$HOME/.octos/skills` may or may not exist. The function
    /// must NOT include those paths in its result, regardless of whether
    /// the directories exist on disk.
    #[test]
    #[allow(unsafe_code)]
    fn plugin_dirs_from_project_drops_legacy_home_rooted_globals() {
        // Serialize env mutation so parallel tests don't fight over HOME.
        let _g = HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let tmp = tempfile::tempdir().unwrap();
        let fake_home = tmp.path();
        let project_dir = fake_home.join("octos-home");
        std::fs::create_dir_all(&project_dir).unwrap();

        // Plant legacy HOME-rooted globals that the OLD scan list would have
        // included. The new scan list MUST NOT include them.
        let legacy_skills = fake_home.join(".octos").join("skills");
        let legacy_plugins = fake_home.join(".octos").join("plugins");
        std::fs::create_dir_all(&legacy_skills).unwrap();
        std::fs::create_dir_all(&legacy_plugins).unwrap();

        // Pivot HOME for the duration of this assertion. dirs::home_dir()
        // honors HOME on Unix; we restore the original after the check.
        let original_home = std::env::var_os("HOME");
        // SAFETY: serialized by HOME_ENV_LOCK above; restored on both the
        // success and panic-unwind paths below.
        unsafe { std::env::set_var("HOME", fake_home) };

        let scan = Config::plugin_dirs_from_project(&project_dir);

        // Restore HOME before assertions so a panic doesn't leak the override.
        // SAFETY: see above.
        match original_home {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }

        assert!(
            !scan.contains(&legacy_skills),
            "`~/.octos/skills` must no longer be scanned; got: {scan:?}"
        );
        assert!(
            !scan.contains(&legacy_plugins),
            "`~/.octos/plugins` must no longer be scanned; got: {scan:?}"
        );
    }

    /// Section B: `plugins.require_signed` deserializes from the new
    /// `[plugins]` config block. Default is `false` (backward compatible).
    #[test]
    fn plugins_require_signed_deserialize_explicit_true() {
        let json = r#"{"plugins": {"require_signed": true}}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert!(config.plugins.require_signed);
    }

    #[test]
    fn plugins_require_signed_defaults_to_false_when_absent() {
        let json = r#"{"provider": "anthropic"}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert!(
            !config.plugins.require_signed,
            "missing `plugins` block must default to `require_signed = false` \
             (backward compat)"
        );
    }

    #[test]
    fn plugins_require_signed_defaults_to_false_when_block_empty() {
        let json = r#"{"plugins": {}}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert!(!config.plugins.require_signed);
    }

    /// Section A: `<octos_home>/plugins` and `<octos_home>/skills`
    /// (deployment-scoped, not HOME-rooted) MUST still be scanned. M11-F
    /// REG-5 added them so admin-installed plugins are visible to every
    /// profile.
    #[test]
    fn plugin_dirs_from_project_keeps_deployment_scoped_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("octos-home");
        let project_plugins = project_dir.join("plugins");
        let project_skills = project_dir.join("skills");
        std::fs::create_dir_all(&project_plugins).unwrap();
        std::fs::create_dir_all(&project_skills).unwrap();

        let scan = Config::plugin_dirs_from_project(&project_dir);

        assert!(
            scan.contains(&project_plugins),
            "`<octos_home>/plugins` must still be scanned; got: {scan:?}"
        );
        assert!(
            scan.contains(&project_skills),
            "`<octos_home>/skills` must still be scanned; got: {scan:?}"
        );
    }

    #[test]
    fn should_default_base_domain_to_none_when_absent() {
        // Backward compat: existing configs without `base_domain` must
        // deserialize to `None` so read sites fall back to the legacy
        // `crew.ominix.io` default.
        let json = r#"{"provider": "anthropic"}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert!(config.base_domain.is_none());
    }

    #[test]
    fn test_validate_max_history_out_of_range() {
        let config = Config {
            gateway: Some(GatewayConfig {
                channels: vec![],
                max_history: 0,
                ..Default::default()
            }),
            ..Default::default()
        };
        let warnings = config.validate();
        assert!(warnings.iter().any(|w| w.contains("out of range")));
    }

    /// Default `AdaptiveRoutingConfig` carries `auto_escalation.enabled = true`
    /// so out-of-the-box installs get the latency-feedback loop.
    #[test]
    fn auto_escalation_defaults_match_router_defaults() {
        let cfg = AdaptiveRoutingConfig::default();
        assert!(cfg.auto_escalation.enabled);
        let llm_cfg = octos_llm::AutoEscalationConfig::from(&cfg.auto_escalation);
        assert!(llm_cfg.enabled);
        assert_eq!(llm_cfg.latency_ceiling_ms, 8_000);
        assert!((llm_cfg.recovery_factor - 0.6).abs() < f64::EPSILON);
        assert_eq!(llm_cfg.slow_trigger, 3);
    }

    /// Missing `auto_escalation` block in JSON resolves to the in-code
    /// defaults instead of disabling the feature.
    #[test]
    fn auto_escalation_missing_block_uses_defaults() {
        let json = r#"{
            "enabled": true,
            "mode": "lane"
        }"#;
        let cfg: AdaptiveRoutingConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.auto_escalation.enabled);
        assert_eq!(cfg.auto_escalation.latency_ceiling_ms, 8_000);
    }

    /// Operators can disable the feature explicitly.
    #[test]
    fn auto_escalation_can_be_disabled() {
        let json = r#"{
            "enabled": true,
            "mode": "lane",
            "auto_escalation": { "enabled": false }
        }"#;
        let cfg: AdaptiveRoutingConfig = serde_json::from_str(json).unwrap();
        assert!(!cfg.auto_escalation.enabled);
    }

    #[test]
    fn voice_config_defaults_tts_provider_to_auto() {
        let cfg = VoiceConfig::default();
        assert_eq!(cfg.tts_provider, "auto");
        assert_eq!(cfg.default_voice, "vivian");
    }

    #[test]
    fn voice_config_tts_provider_defaults_when_omitted() {
        // A profile that sets only some voice fields still gets a valid route.
        let json = r#"{ "default_voice": "doubao" }"#;
        let cfg: VoiceConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.tts_provider, "auto");
        assert_eq!(cfg.default_voice, "doubao");
    }

    #[test]
    fn voice_config_tts_provider_roundtrips() {
        let json = r#"{ "tts_provider": "sovits" }"#;
        let cfg: VoiceConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.tts_provider, "sovits");
    }

    #[test]
    fn should_deserialize_approval_policy_when_present() {
        let json = r#"{
            "provider": "anthropic",
            "approval_policy": {
                "default": "allow",
                "rules": [{
                    "tools": ["shell"],
                    "require_approval": true,
                    "risk_level": "critical",
                    "authorized_approvers": ["@alice:example.org"],
                    "expires_in_secs": 300,
                    "on_timeout": "notify"
                }]
            }
        }"#;
        let config: Config = serde_json::from_str(json).unwrap();
        let policy = config.approval_policy.as_ref().unwrap();
        assert_eq!(policy.rules.len(), 1);
        assert_eq!(policy.rules[0].tools, vec!["shell"]);
        assert!(config.validate_approval_policy().is_ok());

        let runtime = policy.to_runtime_rules();
        assert!(runtime.matching_rule("shell").is_some());
        assert!(runtime.matching_rule("read_file").is_none());
    }

    #[test]
    fn should_expand_env_vars_in_authorized_approvers() {
        // Use an env var reliably present in the test environment instead of
        // mutating the environment (workspace is `deny(unsafe_code)`, and
        // std::env::set_var is unsafe under edition 2024).
        let var = if std::env::var("HOME").is_ok() {
            "HOME"
        } else {
            "PATH"
        };
        let expected = std::env::var(var).unwrap();
        let mut config = Config {
            approval_policy: Some(ApprovalPolicyConfig {
                default: ApprovalPolicyDefault::Allow,
                rules: vec![ApprovalRuleConfig {
                    tools: vec!["shell".into()],
                    require_approval: true,
                    risk_level: ApprovalPolicyRiskLevel::Critical,
                    authorized_approvers: vec![format!("${{{var}}}")],
                    expires_in_secs: 300,
                    on_timeout: ApprovalPolicyTimeoutBehavior::Notify,
                }],
            }),
            ..Default::default()
        };
        config.expand_env_vars();
        assert_eq!(
            config.approval_policy.unwrap().rules[0].authorized_approvers,
            vec![expected],
            "${{VAR}} in authorized_approvers must be expanded before validation"
        );
    }

    #[test]
    fn should_reject_approval_policy_when_rule_invalid() {
        let base = ApprovalRuleConfig {
            tools: vec!["shell".into()],
            require_approval: true,
            risk_level: ApprovalPolicyRiskLevel::Critical,
            authorized_approvers: vec!["@alice:example.org".into()],
            expires_in_secs: 300,
            on_timeout: ApprovalPolicyTimeoutBehavior::Notify,
        };
        let config_with = |rule: ApprovalRuleConfig| Config {
            approval_policy: Some(ApprovalPolicyConfig {
                default: ApprovalPolicyDefault::Allow,
                rules: vec![rule],
            }),
            ..Default::default()
        };

        let mut rule = base.clone();
        rule.tools.clear();
        assert!(config_with(rule).validate_approval_policy().is_err());

        let mut rule = base.clone();
        rule.require_approval = false;
        assert!(config_with(rule).validate_approval_policy().is_err());

        let mut rule = base.clone();
        rule.authorized_approvers.clear();
        assert!(config_with(rule).validate_approval_policy().is_err());

        let mut rule = base.clone();
        rule.expires_in_secs = 0;
        assert!(config_with(rule).validate_approval_policy().is_err());

        assert!(config_with(base).validate_approval_policy().is_ok());
    }

    #[test]
    fn per_profile_override_replaces_only_the_default_voice() {
        // A per-user timbre choice overrides default_voice but leaves the
        // platform-level route/ASR settings intact.
        let base = VoiceConfig {
            tts_provider: "sovits".into(),
            asr_language: Some("zh".into()),
            ..VoiceConfig::default()
        };
        let got = base.with_default_voice_override(Some("yangmi"));
        assert_eq!(got.default_voice, "yangmi");
        assert_eq!(got.tts_provider, "sovits"); // platform setting preserved
        assert_eq!(got.asr_language.as_deref(), Some("zh"));
    }

    #[test]
    fn per_profile_override_ignores_empty_or_absent_choice() {
        let base = VoiceConfig {
            default_voice: "doubao".into(),
            ..VoiceConfig::default()
        };
        assert_eq!(
            base.clone().with_default_voice_override(None).default_voice,
            "doubao"
        );
        assert_eq!(
            base.with_default_voice_override(Some("")).default_voice,
            "doubao"
        );
    }

    #[test]
    fn should_override_tts_provider_when_some_nonempty() {
        let cfg = VoiceConfig::default().with_tts_provider_override(Some("cloud"));
        assert_eq!(cfg.tts_provider, "cloud");
    }

    #[test]
    fn should_keep_tts_provider_when_override_none_or_empty() {
        let base = VoiceConfig::default();
        let kept = base.clone().with_tts_provider_override(None);
        assert_eq!(kept.tts_provider, base.tts_provider);
        let kept2 = base.clone().with_tts_provider_override(Some(""));
        assert_eq!(kept2.tts_provider, base.tts_provider);
    }

    #[test]
    fn should_override_cloud_when_some() {
        let cloud = CloudTtsConfig {
            appid: Some("123".into()),
            ..Default::default()
        };
        let cfg = VoiceConfig::default().with_cloud_override(Some(&cloud));
        assert_eq!(cfg.cloud.unwrap().appid.as_deref(), Some("123"));
    }

    #[test]
    fn should_roundtrip_cloud_tts_config_serde() {
        let json = r#"{ "cloud": { "appid": "a", "voice": "BV700" } }"#;
        let cfg: VoiceConfig = serde_json::from_str(json).unwrap();
        let cloud = cfg.cloud.unwrap();
        assert_eq!(cloud.appid.as_deref(), Some("a"));
        assert_eq!(cloud.voice.as_deref(), Some("BV700"));
        assert_eq!(cloud.cluster, None);
    }

    #[test]
    fn should_never_serialize_cloud_token() {
        let cloud = CloudTtsConfig {
            appid: Some("a".into()),
            token: Some("supersecret".into()),
            ..Default::default()
        };
        let json = serde_json::to_string(&cloud).unwrap();
        assert!(
            !json.contains("token"),
            "token key must not serialize: {json}"
        );
        assert!(
            !json.contains("supersecret"),
            "token value must not leak: {json}"
        );
    }

    #[test]
    fn should_redact_cloud_token_in_debug() {
        let cloud = CloudTtsConfig {
            token: Some("supersecret".into()),
            ..Default::default()
        };
        let dbg = format!("{cloud:?}");
        assert!(
            !dbg.contains("supersecret"),
            "Debug must redact token: {dbg}"
        );
        assert!(dbg.contains("redacted"));
    }

    #[test]
    fn should_resolve_cloud_token_from_env_vars() {
        let mut env = std::collections::HashMap::new();
        env.insert("VOLC_TTS_TOKEN".to_string(), "T-abc".to_string());
        let vc = VoiceConfig {
            cloud: Some(CloudTtsConfig {
                appid: Some("1".into()),
                ..Default::default()
            }),
            ..Default::default()
        }
        .with_cloud_token_from_env(&env);
        assert_eq!(vc.cloud.unwrap().token.as_deref(), Some("T-abc"));
    }

    #[test]
    fn should_not_resolve_cloud_token_when_no_cloud_config() {
        let mut env = std::collections::HashMap::new();
        env.insert("VOLC_TTS_TOKEN".to_string(), "T-abc".to_string());
        let vc = VoiceConfig::default().with_cloud_token_from_env(&env);
        assert!(vc.cloud.is_none());
    }

    #[test]
    fn should_overlay_profile_tts_provider_over_host_voice() {
        let host = VoiceConfig {
            tts_provider: "auto".into(),
            ..Default::default()
        };
        let overridden = host
            .with_tts_provider_override(Some("cloud"))
            .with_cloud_override(Some(&CloudTtsConfig {
                appid: Some("42".into()),
                ..Default::default()
            }));
        assert_eq!(overridden.tts_provider, "cloud");
        assert_eq!(overridden.cloud.unwrap().appid.as_deref(), Some("42"));
    }

    // --- memory refresh flag ---

    #[test]
    fn should_default_refresh_off_when_memory_absent_or_empty() {
        assert!(!MemoryConfig::refresh_enabled(None));
        let empty: MemoryConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(!MemoryConfig::refresh_enabled(Some(&empty)));
        let refresh_empty: MemoryConfig =
            serde_json::from_value(serde_json::json!({"refresh": {}})).unwrap();
        assert!(!MemoryConfig::refresh_enabled(Some(&refresh_empty)));
    }

    #[test]
    fn should_enable_refresh_when_config_sets_it() {
        let cfg: MemoryConfig =
            serde_json::from_value(serde_json::json!({"refresh": {"enabled": true}})).unwrap();
        assert!(MemoryConfig::refresh_enabled(Some(&cfg)));
        // max_inject_tokens keeps its default independently.
        assert_eq!(
            MemoryConfig::effective_max_inject_tokens(Some(&cfg)),
            octos_memory::DEFAULT_MAX_INJECT_TOKENS
        );
    }
}
