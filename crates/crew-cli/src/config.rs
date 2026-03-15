//! Configuration file support for crew CLI.

use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};

/// Current config version.
const CURRENT_CONFIG_VERSION: u32 = 1;

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

    /// Override auto-detected model behavior hints for the OpenAI provider.
    /// Useful for custom/unknown models behind OpenAI-compatible proxies.
    #[serde(default)]
    pub model_hints: Option<crew_llm::openai::ModelHints>,

    /// API protocol type: "openai" (default) or "anthropic".
    /// When set to "anthropic", the Anthropic Messages API format is used
    /// regardless of the provider name (for Anthropic-compatible proxies).
    #[serde(default)]
    pub api_type: Option<String>,

    /// Gateway configuration (optional).
    #[serde(default)]
    pub gateway: Option<GatewayConfig>,

    /// MCP server configurations.
    #[serde(default)]
    pub mcp_servers: Vec<crew_agent::McpServerConfig>,

    /// Sandbox configuration.
    #[serde(default)]
    pub sandbox: crew_agent::SandboxConfig,

    /// Tool access policy (allow/deny lists with group and wildcard support).
    #[serde(default)]
    pub tool_policy: Option<crew_agent::ToolPolicy>,

    /// Per-provider tool policies. Key = model ID or provider name prefix.
    /// Example: `{"gemini": {"deny": ["diff_edit"]}}`.
    #[serde(default)]
    pub tool_policy_by_provider: std::collections::HashMap<String, crew_agent::ToolPolicy>,

    /// Embedding configuration for hybrid memory search.
    #[serde(default)]
    pub embedding: Option<EmbeddingConfig>,

    /// Fallback models for provider failover chain.
    /// When the primary provider fails with a retriable error, the next model is tried.
    #[serde(default)]
    pub fallback_models: Vec<FallbackModel>,

    /// Maximum agent iterations per message (overridden by --max-iterations).
    #[serde(default)]
    pub max_iterations: Option<u32>,

    /// Lifecycle hooks for agent events.
    #[serde(default)]
    pub hooks: Vec<crew_agent::HookConfig>,

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

    /// Dashboard user authentication configuration (email OTP).
    /// When set, enables multi-user login via email verification codes.
    #[cfg(feature = "api")]
    #[serde(default)]
    pub dashboard_auth: Option<crate::otp::DashboardAuthConfig>,

    /// Monitor configuration for watchdog auto-restart and alerts.
    #[cfg(feature = "api")]
    #[serde(default)]
    pub monitor: Option<MonitorConfig>,
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
    pub model_hints: Option<crew_llm::openai::ModelHints>,
    /// API protocol type: "openai" or "anthropic". Overrides provider default.
    #[serde(default)]
    pub api_type: Option<String>,
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
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            api_url: None,
            auto_asr: true,
            auto_tts: true,
            default_voice: default_voice_preset(),
            asr_language: None,
        }
    }
}

fn voice_default_true() -> bool {
    true
}
fn default_voice_preset() -> String {
    "vivian".to_string()
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

impl From<AdaptiveRoutingMode> for crew_llm::AdaptiveMode {
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
        }
    }
}

impl From<&AdaptiveRoutingConfig> for crew_llm::AdaptiveConfig {
    fn from(c: &AdaptiveRoutingConfig) -> Self {
        Self {
            failure_threshold: c.failure_threshold,
            latency_threshold_ms: c.latency_threshold_ms,
            error_rate_threshold: c.error_rate_threshold,
            probe_probability: c.probe_probability,
            probe_interval_secs: c.probe_interval_secs,
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
    /// Scans both `.crew/plugins/` (legacy) and `.crew/skills/` (unified packages).
    /// Skill packages that include a `manifest.json` are auto-discovered as tool
    /// providers by `PluginLoader` (packages without manifest.json are skipped).
    /// Resolve plugin/skill directories from a project directory (e.g. `~/.crew`).
    ///
    /// The `project_dir` is typically `crew_home` (for managed gateways) or
    /// `cwd/.crew` (for standalone `crew chat`). This is intentionally decoupled
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
        let bundled = project_dir.join(crew_agent::bootstrap::BUNDLED_APP_SKILLS_DIR);
        if bundled.exists() {
            dirs.push(bundled);
        }
        // Note: platform-skills/ (voice, etc.) are admin-only — loaded explicitly in serve.rs
        if let Some(home) = dirs::home_dir() {
            let global_plugins = home.join(".crew").join("plugins");
            if global_plugins.exists() {
                dirs.push(global_plugins);
            }
            let global_skills = home.join(".crew").join("skills");
            if global_skills.exists() {
                dirs.push(global_skills);
            }
        }
        // Extra dirs from CREW_SKILLS_PATH env var (colon-separated)
        if let Ok(extra) = std::env::var("CREW_SKILLS_PATH") {
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

/// Message queue mode for handling messages arriving during active agent runs.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum QueueMode {
    /// Process queued messages one at a time (FIFO).
    Followup,
    /// Concatenate queued messages from the same session into one before processing.
    Collect,
    /// Keep only the latest message, discard older queued messages.
    Steer,
    /// Cancel the current run and process the new message immediately.
    Interrupt,
    /// If the current LLM call exceeds the patience threshold and a new message
    /// arrives, spawn a full agent task for the new message concurrently.
    /// Both results are delivered to the user. Default behavior.
    #[default]
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

impl Config {
    /// Load config from file, returns default if not found.
    pub fn load(cwd: &Path) -> Result<Self> {
        // Try project-local config first
        let local_config = cwd.join(".crew").join("config.json");
        if local_config.exists() {
            return Self::from_file(&local_config);
        }

        // Try global config
        if let Some(global_config) = Self::global_config_path() {
            if global_config.exists() {
                return Self::from_file(&global_config);
            }
        }

        // No config found, use defaults
        Ok(Self::default())
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

        // Log if migration changed something (don't silently rewrite user's config)
        if migrated {
            tracing::info!(
                path = %path.display(),
                version = CURRENT_CONFIG_VERSION,
                "Config file needs migration to version {}. Run `crew init` to update.",
                CURRENT_CONFIG_VERSION
            );
        }

        Ok(config)
    }

    /// Get global config path (~/.config/crew/config.json).
    pub fn global_config_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("crew").join("config.json"))
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
        // Check auth store first.
        if let Ok(store) = crate::auth::AuthStore::load() {
            if let Some(cred) = store.get(provider) {
                if !cred.is_expired() {
                    return Ok(cred.access_token.clone());
                }
            }
        }

        // Fall back to environment variable.
        let env_var = self.api_key_env.clone().unwrap_or_else(|| {
            crew_llm::registry::lookup(provider)
                .and_then(|e| e.api_key_env)
                .map(String::from)
                .unwrap_or_else(|| format!("{}_API_KEY", provider.to_uppercase()))
        });

        std::env::var(&env_var).wrap_err_with(|| {
            format!("{env_var} not set. Run `crew auth login -p {provider}` or set the env var")
        })
    }

    /// Validate the configuration, returning any warnings.
    #[allow(clippy::manual_map)]
    pub fn validate(&self) -> Vec<String> {
        let mut warnings = Vec::new();

        // Check provider is valid
        if let Some(ref provider) = self.provider {
            if crew_llm::registry::lookup(provider).is_none() {
                let valid = crew_llm::registry::all_names();
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
                "slack",
                "whatsapp",
                "email",
                "feishu",
                "twilio",
                "wecom",
                "wecom-bot",
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
        let provider = self.provider.as_deref().unwrap_or("anthropic");
        if self.get_api_key(provider).is_err() {
            let env_var = self.api_key_env.clone().unwrap_or_else(|| {
                crew_llm::registry::lookup(provider)
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
    crew_llm::registry::detect_provider(model)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
