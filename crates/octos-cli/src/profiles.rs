//! User profile management for multi-user deployments.
//!
//! Each profile is a named configuration bundle that defines an LLM provider,
//! channel credentials, and gateway settings. Profiles are stored as individual
//! JSON files in `~/.octos/profiles/`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use eyre::{Result, WrapErr, bail};
use serde::{Deserialize, Deserializer, Serialize};

use crate::config::{ChannelEntry, CloudTtsConfig, Config, FallbackModel, GatewayConfig};

pub const MAX_SUB_ACCOUNTS_PER_PARENT: usize = 10;
pub(crate) const HOST_ASR_LANGUAGE_ENV: &str = "OCTOS_HOST_ASR_LANGUAGE";

pub const SUPPORTED_ASR_LANGUAGES: &[&str] = &[
    "Chinese",
    "English",
    "Cantonese",
    "Arabic",
    "German",
    "French",
    "Spanish",
    "Portuguese",
    "Indonesian",
    "Italian",
    "Korean",
    "Russian",
    "Thai",
    "Vietnamese",
    "Japanese",
    "Turkish",
    "Hindi",
    "Malay",
    "Dutch",
    "Swedish",
    "Danish",
    "Finnish",
    "Polish",
    "Czech",
    "Filipino",
    "Persian",
    "Greek",
    "Romanian",
    "Hungarian",
    "Macedonian",
];

pub fn canonical_asr_language(language: &str) -> Option<&'static str> {
    let requested = language.trim();
    match requested.to_ascii_lowercase().as_str() {
        "auto" => return Some("auto"),
        "zh" | "zh-cn" | "zh-hans" => return Some("Chinese"),
        "en" | "en-us" | "en-gb" => return Some("English"),
        _ => {}
    }
    SUPPORTED_ASR_LANGUAGES
        .iter()
        .copied()
        .find(|supported| supported.eq_ignore_ascii_case(requested))
}

/// Resolve the optional language hint sent to ASR. A per-profile setting wins
/// over the serve-level default. `None` and `auto` preserve auto-detection;
/// legacy ISO-style aliases are canonicalized. Unknown inherited values are
/// passed through unchanged for backward compatibility with custom engines.
pub fn effective_asr_language(
    profile_override: Option<&str>,
    serve_default: Option<&str>,
) -> Option<String> {
    let selected = profile_override.or(serve_default)?.trim();
    if selected.eq_ignore_ascii_case("auto") {
        return None;
    }

    Some(
        canonical_asr_language(selected)
            .unwrap_or(selected)
            .to_string(),
    )
}

/// Resolve the effective ASR language for a profile from durable storage.
/// Reading the store on each call is intentional: a Settings save must affect
/// the next AppUI or gateway utterance without restarting the process.
pub fn effective_profile_asr_language(
    profile_store: Option<&ProfileStore>,
    profile_id: Option<&str>,
    serve_default: Option<&str>,
) -> Result<Option<String>> {
    let profile_override = match (profile_store, profile_id) {
        (Some(_), Some(profile_id)) if profile_id == octos_core::MAIN_PROFILE_ID => None,
        (Some(store), Some(profile_id)) => {
            let profile = store
                .get(profile_id)?
                .ok_or_else(|| eyre::eyre!("profile '{profile_id}' not found"))?;
            let profile = store.resolve_runtime_profile(&profile);
            profile.config.asr_language
        }
        _ => None,
    };

    Ok(effective_asr_language(
        profile_override.as_deref(),
        serve_default,
    ))
}

fn deserialize_profile_asr_language<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    value
        .map(|language| {
            canonical_asr_language(&language)
                .map(ToOwned::to_owned)
                .ok_or_else(|| {
                    serde::de::Error::custom(format!("unsupported ASR language '{language}'"))
                })
        })
        .transpose()
}

fn deserialize_profile_asr_language_patch<'de, D>(
    deserializer: D,
) -> std::result::Result<PatchField<String>, D::Error>
where
    D: Deserializer<'de>,
{
    match Option::<String>::deserialize(deserializer)? {
        None => Ok(PatchField::Clear),
        Some(language) => canonical_asr_language(&language)
            .map(|canonical| PatchField::Value(canonical.to_string()))
            .ok_or_else(|| {
                serde::de::Error::custom(format!("unsupported ASR language '{language}'"))
            }),
    }
}

/// A user profile with all configuration needed to run a gateway.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserProfile {
    /// Unique identifier (slug: lowercase alphanumeric + hyphens).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Public host slug used for inbound routing.
    ///
    /// When present, external host routing resolves this slug to the internal
    /// immutable profile ID. Top-level profiles may leave it unset to fall back
    /// to their internal ID. Sub-accounts are expected to set it explicitly at
    /// creation time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_subdomain: Option<String>,
    /// Whether this profile's gateway should auto-start with the server.
    #[serde(default)]
    pub enabled: bool,
    /// Data directory override. Default: `~/.octos/profiles/{id}/data`
    #[serde(default)]
    pub data_dir: Option<String>,
    /// If set, this profile is a sub-account of the given parent profile.
    /// Sub-accounts inherit the parent's LLM contract and low-level env vars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Inline configuration.
    pub config: ProfileConfig,
    /// When this profile was created.
    pub created_at: DateTime<Utc>,
    /// When this profile was last modified.
    pub updated_at: DateTime<Utc>,
}

/// LLM and gateway configuration for a profile.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProfileConfig {
    /// First-class structured LLM selection contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm: Option<LlmProfileConfig>,
    /// Named provider lanes for per-node pipeline routing (e.g. the
    /// `deep_research` pipeline's `cheap`/`strong` nodes) and sub-agent model
    /// selection. These are ISOLATED from the primary coding provider: the serve
    /// path builds a `ProviderRouter` from these entries ONLY (never the coding
    /// primary/fallbacks), so a research-lane failover trips its own circuit
    /// breakers and can never disturb the coding conversation's provider or its
    /// KV/prompt cache. Empty by default (pipeline nodes then use the shared
    /// coding provider, unchanged behavior).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sub_providers: Vec<crate::config::SubProviderConfig>,
    /// MCP servers to attach to this profile's agent (e.g. the OLP-MCP
    /// outer-loop server). Loaded into the runtime `Config.mcp_servers` by
    /// `config_from_profile` — before OLP #29 S2b the field was missing here
    /// and the runtime field was hard-zeroed, so a profile-level
    /// `[[mcp_servers]]` block silently never registered any tools.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<octos_agent::McpServerConfig>,
    /// Per-tenant reply-voice (TTS timbre) choice. Voice route/ASR settings stay
    /// platform-level on the serve config; only the chosen timbre is per-user.
    /// Applied at profile bootstrap over the shared `VoiceConfig.default_voice`
    /// (see `VoiceConfig::with_default_voice_override`). `None` → inherit the
    /// serve default. Set by `PUT /api/my/voice`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_default: Option<String>,
    /// Per-profile TTS route override (`auto`/`local`/`cloud`). `None` →
    /// inherit the serve-level `VoiceConfig.tts_provider`. Applied in
    /// `runtime/profile.rs` via `VoiceConfig::with_tts_provider_override`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tts_provider: Option<String>,
    /// Per-profile non-secret cloud (Volcano) TTS settings. The token rides
    /// `env_vars["VOLC_TTS_TOKEN"]`. `None` → inherit serve / env defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tts_cloud: Option<CloudTtsConfig>,
    /// Per-profile ASR language override. `None` inherits the serve-level voice
    /// setting, `"auto"` explicitly enables auto-detection, and named values
    /// are canonical Qwen3-ASR language names.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_profile_asr_language"
    )]
    pub asr_language: Option<String>,
    /// Coding review specialist template. When omitted, `/review`
    /// uses the server's built-in default specialists. Operators may
    /// configure this per profile to change the native reviewer fanout
    /// without changing code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewConfig>,
    /// Per-profile memory subsystem settings (e.g. the token budget for the
    /// memory block injected into the system prompt). `None` → defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<crate::config::MemoryConfig>,
    /// Search provider contract for product-level search behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<SearchConfig>,
    /// Deep crawl defaults for deterministic page settling and output bounds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deep_crawl: Option<DeepCrawlConfig>,
    /// First-party app configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apps: Option<AppsConfig>,
    /// Home dashboard UI configuration. The backend stores this as opaque JSON
    /// because Home is a web-owned surface; typed validation lives in octos-web.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<serde_json::Value>,
    /// Robotics runtime configuration (heartbeat + sensor context injection).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub robot: Option<RobotConfig>,
    /// Channel configurations.
    #[serde(default)]
    pub channels: Vec<ChannelCredentials>,
    /// Gateway-specific settings.
    #[serde(default)]
    pub gateway: GatewaySettings,
    /// Email sending configuration (SMTP or Feishu/Lark).
    #[serde(default)]
    pub email: Option<EmailSettings>,
    /// Smart-home bridge integration (self-hosted/LAN-reachable bridge only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smart_home: Option<SmartHomeConfig>,
    /// API protocol type: "openai" or "anthropic". Overrides provider default.
    #[serde(default)]
    pub api_type: Option<String>,
    /// Low-level environment overrides only (API keys, secrets, escape hatches).
    /// Product behavior should live in typed config sections above.
    /// Keys are env var names, values are the actual secrets.
    #[serde(default)]
    pub env_vars: HashMap<String, String>,
    /// Lifecycle hooks for agent events (per-profile).
    #[serde(default)]
    pub hooks: Vec<octos_agent::HookConfig>,
    /// #2168: per-profile tool-visibility policy (allow / deny / require_tags,
    /// with `group:*` support). Projected into `Config.tool_policy`, which the
    /// serve path already applies — it just had no way to be set from a
    /// profile, so a serve / UserProfile session could not slim its roster the
    /// way the built-in `coding` profile does (#2133). `None` = no filtering.
    ///
    /// NOTE on the mechanism: this goes through `ToolRegistry::apply_policy`
    /// (deny-wins, then an allow-list `retain`), NOT the built-in profile's
    /// `filter_by_profile`. The difference: `apply_policy` has NO `spawn_only`
    /// carve-out (by design, so a `deny: ["run_pipeline"]` actually works). So
    /// an `allow` list here also drops the per-session serve tools that are not
    /// in it — `run_pipeline` (spawn_only), `message`, `send_file`,
    /// `send_app_card`, `read_task_output`, `check_background_tasks`, `recall`,
    /// `cron`. For a lean *coding* surface that is fine; for a general serve
    /// profile prefer a **`deny`** list of the heavy web/research/media tools,
    /// which keeps every coding + channel + task tool intact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_policy: Option<octos_agent::ToolPolicy>,
    /// Human-approval rules for tool calls requiring a human decision
    /// (per-profile; see `docs/ROBRIX-PHASE4-APPROVAL-FLOW-ADR.md`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<crate::config::ApprovalPolicyConfig>,
    /// Admin mode: when true, gateway registers only admin management tools
    /// (no shell, file, web, browser tools). Used for the admin bot profile.
    #[serde(default)]
    pub admin_mode: bool,
    /// #1774: opt-in post-edit formatting (rustfmt/prettier/black/gofmt)
    /// after successful edit_file/write_file/diff_edit. Default OFF.
    #[serde(default)]
    pub format_after_edit: bool,
    /// #1768: opt-in git-backed workspace snapshots before mutating tools
    /// (`snapshots.enabled` + `keep_last`). Default OFF.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshots: Option<octos_agent::SnapshotConfig>,
    /// Sandbox configuration for tool isolation.
    #[serde(default)]
    pub sandbox: octos_agent::SandboxConfig,
    /// Adaptive routing configuration (QoS weights, mode, etc.).
    #[serde(default)]
    pub adaptive_routing: Option<crate::config::AdaptiveRoutingConfig>,
    /// Optional cost / provenance budget policy for swarm dispatches
    /// (M7.4). Absent or empty => no enforcement; the ledger still
    /// records attributions so operators can audit spend retroactively.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_budget: Option<octos_agent::CostBudgetPolicy>,
    /// Matrix-specific profile config (e.g. swarm supervisor rooms).
    ///
    /// Absent → behaves exactly like pre-M7.3 Matrix deployments. Present →
    /// enables Matrix-as-supervisor-UI via agent puppets (see
    /// [`MatrixProfileConfig`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matrix: Option<MatrixProfileConfig>,
    /// Content-classified smart routing configuration (M6.6).
    /// Missing config defaults to `enabled: false` (invariant #3 of issue #493).
    #[serde(default)]
    pub content_routing: Option<octos_llm::RoutingConfig>,
    /// Credential pool configuration (M6.5). Named pools of API keys / OAuth
    /// tokens with persistent cooldowns and rotation strategies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_pool: Option<CredentialPoolConfig>,
    /// Plugin loader policy. Mirrors the top-level `plugins` block in
    /// `config.json` so per-profile gateways can opt into strict signature
    /// enforcement independently of the host-level setting. Default
    /// (`PluginsConfig::default()`) preserves backward compatibility —
    /// unsigned plugins still load with a warning.
    #[serde(default)]
    pub plugins: crate::config::PluginsConfig,
    /// RFC-3 (#1292) — per-topic model lane routing. When set, the
    /// session-actor and the WS turn handler resolve the session's
    /// `topic()` to a [`octos_llm::Lane`] using these overrides on
    /// top of the built-in defaults, then scope the LLM chat call
    /// inside [`octos_llm::with_lane_context`] so the
    /// [`octos_llm::AdaptiveRouter`] narrows its candidate set to
    /// the lane's `(provider, model)` list before scoring.
    ///
    /// Absent / `None` ⇒ pre-RFC-3 behavior: the router uses the
    /// profile-default provider chain unchanged. The built-in lane
    /// defaults still apply for topic prefixes (`slides`, `code`,
    /// `research`, etc.) — the per-profile field only carries
    /// **overrides**.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane_routing: Option<octos_llm::LaneRoutingConfig>,
    /// Skill-layering (v1) selection layer. Merged through
    /// [`ProfileStore::effective_config`] alongside hooks / env / sandbox /
    /// plugins so a profile inherits the operator's default skill selection
    /// and may narrow or extend it.
    ///
    /// `None` ⇒ "no local skills layer" (inherit the defaults' layer, if
    /// any). When both the defaults and the profile omit it, the merge yields
    /// `None` and every discovered skill loads exactly as before this feature
    /// existed (backwards-compatible). See [`ProfileSkillsConfig`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<ProfileSkillsConfig>,
}

/// How a profile selects which discovered skills to load.
///
/// This is ordinary selection, NOT a security policy — a profile may
/// re-enable a skill that an inherited rule disabled (enforced denies would be
/// a separate future policy layer, out of scope for v1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillSelectionMode {
    /// Load every discovered skill except those with an `enabled: false` rule.
    /// This is the default and matches pre-skill-layering behavior.
    #[default]
    AllDiscovered,
    /// Load only skills that have an explicit `enabled: true` rule.
    AllowList,
}

/// A single per-skill selection rule, keyed by the skill's identifier.
///
/// `id` is the skill package identifier: the `manifest.json` `name`/`id`
/// field, which equals the `SKILL.md` `name` and the skill directory name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillRule {
    /// Skill identifier (manifest `name`/`id` == `SKILL.md` `name`).
    pub id: String,
    /// Whether this skill is enabled.
    pub enabled: bool,
}

/// Per-profile skill selection layer.
///
/// Absent (`ProfileConfig::skills == None`) ⇒ [`SkillSelectionMode::AllDiscovered`]
/// with no rules ⇒ every discovered skill loads (backwards-compatible).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileSkillsConfig {
    /// Selection mode. `None` preserves "inherit mode" through the merge
    /// (`profile.mode.or(defaults.mode)`); an absent mode resolves to
    /// [`SkillSelectionMode::AllDiscovered`] at load time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<SkillSelectionMode>,
    /// Per-skill rules. Keyed by `id`; the last rule for a given id wins.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<SkillRule>,
}

impl ProfileSkillsConfig {
    /// The effective selection mode (absent ⇒ [`SkillSelectionMode::AllDiscovered`]).
    pub fn effective_mode(&self) -> SkillSelectionMode {
        self.mode.unwrap_or_default()
    }

    /// Whether a skill with `id` should load under this selection layer.
    ///
    /// Rules are last-wins per id. In [`SkillSelectionMode::AllDiscovered`] a
    /// skill loads unless a rule disables it; in
    /// [`SkillSelectionMode::AllowList`] a skill loads only when a rule
    /// enables it.
    pub fn allows(&self, id: &str) -> bool {
        let last = self.rules.iter().rev().find(|rule| rule.id == id);
        match self.effective_mode() {
            SkillSelectionMode::AllDiscovered => last.map(|rule| rule.enabled).unwrap_or(true),
            SkillSelectionMode::AllowList => last.map(|rule| rule.enabled).unwrap_or(false),
        }
    }

    /// Lower this selection layer into the crate-agnostic
    /// [`octos_agent::SkillFilter`] handed to the plugin loader and the
    /// [`octos_agent::SkillsLoader`]. Rules are collapsed last-wins per id.
    pub fn to_agent_filter(&self) -> octos_agent::SkillFilter {
        let mut last: std::collections::HashMap<&str, bool> = std::collections::HashMap::new();
        for rule in &self.rules {
            last.insert(rule.id.as_str(), rule.enabled);
        }
        match self.effective_mode() {
            SkillSelectionMode::AllDiscovered => octos_agent::SkillFilter::AllExcept(
                last.into_iter()
                    .filter(|(_, enabled)| !*enabled)
                    .map(|(id, _)| id.to_string())
                    .collect(),
            ),
            SkillSelectionMode::AllowList => octos_agent::SkillFilter::Only(
                last.into_iter()
                    .filter(|(_, enabled)| *enabled)
                    .map(|(id, _)| id.to_string())
                    .collect(),
            ),
        }
    }
}

/// Profile-owned review workflow configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewConfig {
    /// Native model-backed specialists to launch for AppUI `review/start`.
    ///
    /// Empty means "use the built-in default template".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub native_specialists: Vec<ReviewSpecialistConfig>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewSpecialistConfig {
    /// Stable suffix used to build the child agent id.
    pub agent_key: String,
    /// Human-facing agent name rendered in AppUI traces.
    pub nickname: String,
    /// Machine-readable review role.
    pub role: String,
    /// Focus text injected into the specialist prompt.
    pub focus: String,
}

/// Search configuration persisted in the profile contract.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchConfig {
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub providers: HashMap<String, SearchProviderConfig>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchProviderConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
}

/// Deep crawl defaults persisted in the profile contract.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeepCrawlConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_settle_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_chars: Option<usize>,
}

/// First-party app configuration persisted in the profile contract.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slides: Option<SlidesAppConfig>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlidesAppConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_theme: Option<String>,
}

/// Robotics-oriented profile configuration.
///
/// Currently only hosts the realtime heartbeat + sensor injection contract
/// added in RP05. Future robotics knobs (e-stop topic, safe-hold behavior)
/// should nest under this struct so a single `robot: null` patch can strip
/// all robotics integration in one step.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RobotConfig {
    /// Realtime heartbeat + sensor-context-injection contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub realtime: Option<octos_agent::RealtimeConfig>,
}

/// Current schema version for [`SwarmSupervisorConfig`].
///
/// Older configs that omit `schema_version` are accepted as v1 via
/// [`default_swarm_supervisor_schema_version`]. Tracks
/// [`octos_agent::SWARM_SUPERVISOR_CONFIG_SCHEMA_VERSION`] — the two MUST
/// stay in lock-step so the agent-side ABI compat checks and the CLI-side
/// profile loader agree on the serialized shape.
pub const SWARM_SUPERVISOR_CONFIG_SCHEMA_VERSION: u32 =
    octos_agent::SWARM_SUPERVISOR_CONFIG_SCHEMA_VERSION;

fn default_swarm_supervisor_schema_version() -> u32 {
    SWARM_SUPERVISOR_CONFIG_SCHEMA_VERSION
}

/// Matrix-specific profile configuration.
///
/// Holds optional Matrix-scoped features that extend the baseline appservice
/// channel; absent fields leave the channel behavior unchanged.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatrixProfileConfig {
    /// Swarm supervisor UI contract — route harness events to per-swarm rooms
    /// and accept supervisor replies as steering input. Absent → disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swarm_supervisor: Option<SwarmSupervisorConfig>,
}

/// Configuration for Matrix-as-supervisor-UI via agent puppets (M7.3).
///
/// When present, each sub-agent in a swarm is surfaced as a Matrix puppet
/// user in a per-swarm room. The human supervisor interacts through any
/// Matrix client (Element, etc.) and replies route back to the addressed
/// puppet as steering input.
///
/// The bot account backing the appservice MUST hold Matrix admin API
/// permissions so it can register puppet users and invite them to rooms.
/// Deployments without admin rights MUST leave this section absent, which
/// preserves the pre-M7.3 Matrix channel behavior exactly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwarmSupervisorConfig {
    /// Durable ABI schema version for this config section.
    ///
    /// See [`SWARM_SUPERVISOR_CONFIG_SCHEMA_VERSION`] for the current value
    /// and `docs/OCTOS_HARNESS_ABI_VERSIONING.md` for per-version field
    /// guarantees. Older configs without this field default to v1.
    #[serde(default = "default_swarm_supervisor_schema_version")]
    pub schema_version: u32,
    /// Matrix localpart prefix used for puppet users (e.g. `"swarm_"` →
    /// `@swarm_s3f1:server`). Scopes puppets out of the shared user
    /// namespace used by baseline bots.
    #[serde(default = "default_swarm_puppet_prefix")]
    pub puppet_prefix: String,
    /// Matrix room alias prefix for per-swarm supervisor rooms (e.g.
    /// `"swarm_"` → `#swarm_s3f1:server`). Aliases are idempotent — re-running
    /// `ensure_swarm_room` returns the same room ID.
    #[serde(default = "default_swarm_room_prefix")]
    pub room_prefix: String,
    /// Matrix user IDs that will be invited to every swarm room as
    /// supervisors. Replies from these users route to the addressed puppet.
    #[serde(default)]
    pub supervisor_user_ids: Vec<String>,
    /// If true, verify the bot account reports `admin: true` on the
    /// homeserver before provisioning puppets. When disabled, the channel
    /// best-effort uses the appservice token for user registration (the
    /// existing Matrix appservice pattern).
    #[serde(default)]
    pub require_admin_api: bool,
}

impl Default for SwarmSupervisorConfig {
    fn default() -> Self {
        Self {
            schema_version: SWARM_SUPERVISOR_CONFIG_SCHEMA_VERSION,
            puppet_prefix: default_swarm_puppet_prefix(),
            room_prefix: default_swarm_room_prefix(),
            supervisor_user_ids: Vec::new(),
            require_admin_api: false,
        }
    }
}

fn default_swarm_puppet_prefix() -> String {
    "swarm_".to_string()
}

fn default_swarm_room_prefix() -> String {
    "swarm_".to_string()
}

/// Credential pool configuration (M6.5).
///
/// Schema-versioned per M4.6 — older profiles default to
/// `schema_version = 1`. A pool entry names a set of secrets (typically API
/// keys) that the runtime rotates under the chosen strategy. The secrets
/// themselves live in `env_vars` under `api_key_env`; only ids / knobs are
/// persisted here.
///
/// Classified `RestartRequired` in `diff_profiles` (see the RP05 pattern) —
/// rotating strategy or pool membership at runtime would require tearing
/// down live provider clients, so the safer default is to restart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialPoolConfig {
    /// Schema version for forward compatibility (M4.6 pattern).
    #[serde(default = "octos_agent::default_credential_pool_config_schema_version")]
    pub schema_version: u32,
    /// Named pools keyed by integration id (e.g. `"anthropic"`, `"openai"`).
    #[serde(default)]
    pub pools: HashMap<String, CredentialPoolEntry>,
}

impl Default for CredentialPoolConfig {
    fn default() -> Self {
        Self {
            schema_version: octos_agent::default_credential_pool_config_schema_version(),
            pools: HashMap::new(),
        }
    }
}

/// Single credential pool definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialPoolEntry {
    /// Rotation strategy identifier: `"fill_first"`, `"round_robin"`,
    /// `"random"`, `"least_used"`. Defaults to `round_robin` when absent.
    #[serde(default = "default_rotation_strategy")]
    pub strategy: String,
    /// Ordered credential ids that belong to this pool. The runtime pairs
    /// each id with an API key env var from `env_vars` via `api_key_env`.
    #[serde(default)]
    pub credential_ids: Vec<String>,
    /// Per-credential env var names (legacy bulk form). When both this and
    /// `credential_ids` are present, `credential_ids` takes priority and
    /// env vars are looked up by id.
    #[serde(default)]
    pub credential_env_vars: Vec<String>,
    /// Default cooldown applied to 429 responses without an explicit
    /// `reset_at` hint. Milliseconds.
    #[serde(default)]
    pub default_cooldown_ms: Option<u64>,
    /// Optional override for the persistent state file. Defaults to
    /// `<data_dir>/credential_pool.redb` per M6.5 spec.
    #[serde(default)]
    pub state_path: Option<String>,
}

impl Default for CredentialPoolEntry {
    fn default() -> Self {
        Self {
            strategy: default_rotation_strategy(),
            credential_ids: Vec::new(),
            credential_env_vars: Vec::new(),
            default_cooldown_ms: None,
            state_path: None,
        }
    }
}

fn default_rotation_strategy() -> String {
    "round_robin".into()
}

#[derive(Debug, Clone, Default, PartialEq)]
pub enum PatchField<T> {
    #[default]
    Absent,
    Clear,
    Value(T),
}

impl<T> PatchField<T> {
    pub fn into_value(self) -> Option<T> {
        match self {
            Self::Value(value) => Some(value),
            Self::Absent | Self::Clear => None,
        }
    }
}

impl<'de, T> Deserialize<'de> for PatchField<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match Option::<T>::deserialize(deserializer)? {
            Some(value) => Self::Value(value),
            None => Self::Clear,
        })
    }
}

/// Partial profile config update from the admin/self-service API.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileConfigPatch {
    #[serde(default)]
    pub llm: PatchField<LlmProfileConfig>,
    #[serde(default)]
    pub review: PatchField<ReviewConfig>,
    #[serde(default)]
    pub search: PatchField<SearchConfig>,
    #[serde(default)]
    pub deep_crawl: PatchField<DeepCrawlConfig>,
    #[serde(default)]
    pub apps: PatchField<AppsConfig>,
    #[serde(default)]
    pub home: PatchField<serde_json::Value>,
    #[serde(default)]
    pub robot: PatchField<RobotConfig>,
    #[serde(default)]
    pub channels: Option<Vec<ChannelCredentials>>,
    #[serde(default)]
    pub gateway: Option<GatewaySettingsPatch>,
    #[serde(default)]
    pub email: PatchField<EmailSettings>,
    #[serde(default)]
    pub smart_home: PatchField<SmartHomeConfig>,
    #[serde(default, deserialize_with = "deserialize_profile_asr_language_patch")]
    pub asr_language: PatchField<String>,
    #[serde(default)]
    pub env_vars: Option<HashMap<String, String>>,
    #[serde(default)]
    pub hooks: Option<Vec<octos_agent::HookConfig>>,
    #[serde(default)]
    pub admin_mode: Option<bool>,
    #[serde(default)]
    pub sandbox: Option<octos_agent::SandboxConfig>,
    #[serde(default)]
    pub adaptive_routing: PatchField<crate::config::AdaptiveRoutingConfig>,
    #[serde(default)]
    pub cost_budget: PatchField<octos_agent::CostBudgetPolicy>,
    #[serde(default)]
    pub matrix: PatchField<MatrixProfileConfig>,
    #[serde(default)]
    pub content_routing: PatchField<octos_llm::RoutingConfig>,
    #[serde(default)]
    pub credential_pool: PatchField<CredentialPoolConfig>,
    #[serde(default)]
    pub plugins: Option<crate::config::PluginsConfig>,
    #[serde(default)]
    pub lane_routing: PatchField<octos_llm::LaneRoutingConfig>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewaySettingsPatch {
    #[serde(default)]
    pub max_history: PatchField<usize>,
    #[serde(default)]
    pub max_iterations: PatchField<u32>,
    #[serde(default)]
    pub system_prompt: PatchField<String>,
    #[serde(default)]
    pub max_concurrent_sessions: PatchField<usize>,
    #[serde(default)]
    pub browser_timeout_secs: PatchField<u64>,
    #[serde(default)]
    pub max_output_tokens: PatchField<u32>,
    #[serde(default)]
    pub watchdog_enabled: PatchField<bool>,
    #[serde(default)]
    pub alerts_enabled: PatchField<bool>,
}

/// Structured LLM contract for a profile.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmProfileConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary: Option<LlmModelSelectionConfig>,
    #[serde(default)]
    pub fallbacks: Vec<LlmModelSelectionConfig>,
}

/// A concrete model selection inside the LLM contract.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmModelSelectionConfig {
    /// Canonical model family / provider family (e.g. "moonshot", "deepseek").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family_id: Option<String>,
    /// Concrete model identifier (e.g. "kimi-k2.5").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    /// Selected provider route for this model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<LlmRouteConfig>,
    /// Optional model behavior hints for custom or proxy-hosted models.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_hints: Option<octos_llm::openai::ModelHints>,
    /// Published output price in USD per million tokens (for routing).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_per_m: Option<f64>,
    /// Whether this is considered a strong model for large tool-heavy runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strong: Option<bool>,
    /// Operator override for the effective context window, in tokens. When
    /// set it takes precedence over BOTH the static catalog and the runtime
    /// probe (#2135): the provider is wrapped in `ContextWindowOverride` as
    /// the outermost layer, so `context_window()` resolves to this value
    /// through the entire runtime stack. Use it to pin a smaller window than
    /// a server advertises (e.g. cap a 262K llama-server at 16384 to bound
    /// KV/compaction) or to correct a mis-probed backend. `None` = defer to
    /// probe/catalog. Applies to the primary and to each fallback
    /// independently. (#2142, split from #2127.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
}

/// A provider route / endpoint choice for one model.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmRouteConfig {
    /// Stable route ID from the catalog (e.g. "official", "autodl", "wisemodel").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_id: Option<String>,
    /// Human-readable route label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Concrete base URL for the selected route. Omitted when the family default
    /// endpoint should be used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// API key env var for this route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Protocol override for this route, e.g. "anthropic" or "responses".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_type: Option<String>,
}

/// Email sending tool configuration for a profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmailSettings {
    /// Provider: "smtp" or "feishu" / "lark".
    pub provider: String,

    // -- SMTP fields --
    #[serde(default)]
    pub smtp_host: Option<String>,
    #[serde(default)]
    pub smtp_port: Option<u16>,
    #[serde(default)]
    pub username: Option<String>,
    /// Env var name holding the SMTP password (legacy).
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
    /// Env var name holding the Feishu app secret (legacy).
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

impl EmailSettings {
    /// Return env var pairs that the `send_email` plugin expects.
    /// `env_vars` is the profile's env_vars map used to resolve `password_env`.
    pub fn to_env_vars(&self, env_vars: &HashMap<String, String>) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if let Some(ref h) = self.smtp_host {
            out.push(("SMTP_HOST".into(), h.clone()));
        }
        if let Some(p) = self.smtp_port {
            out.push(("SMTP_PORT".into(), p.to_string()));
        }
        if let Some(ref u) = self.username {
            out.push(("SMTP_USERNAME".into(), u.clone()));
        }
        if let Some(ref f) = self.from_address {
            out.push(("SMTP_FROM".into(), f.clone()));
        }
        // Resolve password: direct `password` field preferred, then `password_env` lookup
        if let Some(ref pw) = self.password {
            out.push(("SMTP_PASSWORD".into(), pw.clone()));
        } else if let Some(ref pw_env) = self.password_env {
            if let Some(pw_val) = env_vars.get(pw_env) {
                out.push(("SMTP_PASSWORD".into(), pw_val.clone()));
            }
        }
        if let Some(ref id) = self.feishu_app_id {
            out.push(("LARK_APP_ID".into(), id.clone()));
        }
        if let Some(ref secret) = self.feishu_app_secret {
            out.push(("LARK_APP_SECRET".into(), secret.clone()));
        } else if let Some(ref secret_env) = self.feishu_app_secret_env {
            if let Some(secret_val) = env_vars.get(secret_env) {
                out.push(("LARK_APP_SECRET".into(), secret_val.clone()));
            }
        }
        if let Some(ref f) = self.feishu_from_address {
            out.push(("LARK_FROM_ADDRESS".into(), f.clone()));
        }
        if let Some(ref r) = self.feishu_region {
            out.push(("LARK_REGION".into(), r.clone()));
        }
        out
    }
}

/// Smart-home bridge integration for a profile.
///
/// Scope: self-hosted / same-LAN bridges only (e.g. a Home Assistant bridge
/// reachable at `http://192.168.x.x:8787`). Cloud-tenant deployments where the
/// backend isn't on the user's home network are out of scope for now.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SmartHomeConfig {
    /// Base URL of the smart-home bridge (e.g. `http://192.168.1.50:8787`).
    /// Not a secret — left in the clear by `mask_secrets`.
    #[serde(default)]
    pub bridge_url: Option<String>,
    /// Bridge auth token (literal value, preferred over token_env).
    #[serde(default)]
    pub token: Option<String>,
    /// Env var name holding the bridge auth token (legacy/reference pattern).
    #[serde(default)]
    pub token_env: Option<String>,
}

impl SmartHomeConfig {
    /// Return env var pairs that the `smart-home` plugin skill and the
    /// backend's own bridge client expect.
    /// `env_vars` is the profile's env_vars map used to resolve `token_env`.
    pub fn to_env_vars(&self, env_vars: &HashMap<String, String>) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if let Some(ref url) = self.bridge_url {
            out.push(("SMART_HOME_BRIDGE_URL".into(), url.clone()));
        }
        if let Some(ref token) = self.token {
            out.push(("SMART_HOME_BRIDGE_TOKEN".into(), token.clone()));
        } else if let Some(ref token_env) = self.token_env {
            if let Some(token_val) = env_vars.get(token_env) {
                out.push(("SMART_HOME_BRIDGE_TOKEN".into(), token_val.clone()));
            }
        }
        out
    }
}

impl ProfileConfig {
    pub fn primary_llm(&self) -> Option<&LlmModelSelectionConfig> {
        self.llm.as_ref().and_then(|llm| llm.primary.as_ref())
    }

    pub fn primary_provider(&self) -> Option<&str> {
        self.primary_llm()
            .and_then(|selection| selection.family_id.as_deref())
    }

    pub fn primary_model(&self) -> Option<&str> {
        self.primary_llm()
            .and_then(|selection| selection.model_id.as_deref())
    }

    pub fn apply_patch(&mut self, patch: ProfileConfigPatch) {
        match patch.llm {
            PatchField::Absent => {}
            PatchField::Clear => self.llm = None,
            PatchField::Value(llm) => self.llm = Some(llm),
        }
        match patch.review {
            PatchField::Absent => {}
            PatchField::Clear => self.review = None,
            PatchField::Value(review) => self.review = Some(review),
        }
        match patch.search {
            PatchField::Absent => {}
            PatchField::Clear => self.search = None,
            PatchField::Value(search) => self.search = Some(search),
        }
        match patch.deep_crawl {
            PatchField::Absent => {}
            PatchField::Clear => self.deep_crawl = None,
            PatchField::Value(deep_crawl) => self.deep_crawl = Some(deep_crawl),
        }
        match patch.apps {
            PatchField::Absent => {}
            PatchField::Clear => self.apps = None,
            PatchField::Value(apps) => self.apps = Some(apps),
        }
        match patch.home {
            PatchField::Absent => {}
            PatchField::Clear => self.home = None,
            PatchField::Value(home) => self.home = Some(home),
        }
        match patch.robot {
            PatchField::Absent => {}
            PatchField::Clear => self.robot = None,
            PatchField::Value(robot) => self.robot = Some(robot),
        }
        if let Some(channels) = patch.channels {
            self.channels = channels;
        }
        if let Some(gateway) = patch.gateway {
            gateway.apply_to(&mut self.gateway);
        }
        match patch.email {
            PatchField::Absent => {}
            PatchField::Clear => self.email = None,
            PatchField::Value(email) => self.email = Some(email),
        }
        match patch.smart_home {
            PatchField::Absent => {}
            PatchField::Clear => self.smart_home = None,
            PatchField::Value(smart_home) => self.smart_home = Some(smart_home),
        }
        match patch.asr_language {
            PatchField::Absent => {}
            PatchField::Clear => self.asr_language = None,
            PatchField::Value(asr_language) => self.asr_language = Some(asr_language),
        }
        if let Some(env_vars) = patch.env_vars {
            self.env_vars = env_vars;
        }
        if let Some(hooks) = patch.hooks {
            self.hooks = hooks;
        }
        if let Some(admin_mode) = patch.admin_mode {
            self.admin_mode = admin_mode;
        }
        if let Some(sandbox) = patch.sandbox {
            self.sandbox = sandbox;
        }
        match patch.adaptive_routing {
            PatchField::Absent => {}
            PatchField::Clear => self.adaptive_routing = None,
            PatchField::Value(adaptive_routing) => self.adaptive_routing = Some(adaptive_routing),
        }
        match patch.cost_budget {
            PatchField::Absent => {}
            PatchField::Clear => self.cost_budget = None,
            PatchField::Value(cost_budget) => self.cost_budget = Some(cost_budget),
        }
        match patch.matrix {
            PatchField::Absent => {}
            PatchField::Clear => self.matrix = None,
            PatchField::Value(matrix) => self.matrix = Some(matrix),
        }
        match patch.content_routing {
            PatchField::Absent => {}
            PatchField::Clear => self.content_routing = None,
            PatchField::Value(content_routing) => self.content_routing = Some(content_routing),
        }
        match patch.credential_pool {
            PatchField::Absent => {}
            PatchField::Clear => self.credential_pool = None,
            PatchField::Value(credential_pool) => self.credential_pool = Some(credential_pool),
        }
        if let Some(plugins) = patch.plugins {
            self.plugins = plugins;
        }
        match patch.lane_routing {
            PatchField::Absent => {}
            PatchField::Clear => self.lane_routing = None,
            PatchField::Value(lane_routing) => self.lane_routing = Some(lane_routing),
        }

        self.normalize_llm_contract();
    }

    pub fn has_llm_selection(&self) -> bool {
        let mut normalized = self.clone();
        normalized.normalize_llm_contract();
        normalized
            .primary_llm()
            .is_some_and(|primary| primary.family_id.is_some() || primary.model_id.is_some())
    }

    pub fn normalize_llm_contract(&mut self) {
        let Some(mut llm) = self.llm.take() else {
            return;
        };

        if llm
            .primary
            .as_ref()
            .is_some_and(LlmModelSelectionConfig::is_empty)
        {
            llm.primary = None;
        }
        llm.fallbacks.retain(|selection| !selection.is_empty());

        self.llm = if llm.primary.is_none() && llm.fallbacks.is_empty() {
            None
        } else {
            Some(llm)
        };
    }
}

impl GatewaySettingsPatch {
    fn apply_to(self, gateway: &mut GatewaySettings) {
        match self.max_history {
            PatchField::Absent => {}
            PatchField::Clear => gateway.max_history = None,
            PatchField::Value(max_history) => gateway.max_history = Some(max_history),
        }
        match self.max_iterations {
            PatchField::Absent => {}
            PatchField::Clear => gateway.max_iterations = None,
            PatchField::Value(max_iterations) => gateway.max_iterations = Some(max_iterations),
        }
        match self.system_prompt {
            PatchField::Absent => {}
            PatchField::Clear => gateway.system_prompt = None,
            PatchField::Value(system_prompt) => gateway.system_prompt = Some(system_prompt),
        }
        match self.max_concurrent_sessions {
            PatchField::Absent => {}
            PatchField::Clear => gateway.max_concurrent_sessions = None,
            PatchField::Value(max_concurrent_sessions) => {
                gateway.max_concurrent_sessions = Some(max_concurrent_sessions);
            }
        }
        match self.browser_timeout_secs {
            PatchField::Absent => {}
            PatchField::Clear => gateway.browser_timeout_secs = None,
            PatchField::Value(browser_timeout_secs) => {
                gateway.browser_timeout_secs = Some(browser_timeout_secs);
            }
        }
        match self.max_output_tokens {
            PatchField::Absent => {}
            PatchField::Clear => gateway.max_output_tokens = None,
            PatchField::Value(max_output_tokens) => {
                gateway.max_output_tokens = Some(max_output_tokens);
            }
        }
        match self.watchdog_enabled {
            PatchField::Absent => {}
            PatchField::Clear => gateway.watchdog_enabled = None,
            PatchField::Value(watchdog_enabled) => {
                gateway.watchdog_enabled = Some(watchdog_enabled);
            }
        }
        match self.alerts_enabled {
            PatchField::Absent => {}
            PatchField::Clear => gateway.alerts_enabled = None,
            PatchField::Value(alerts_enabled) => {
                gateway.alerts_enabled = Some(alerts_enabled);
            }
        }
    }
}

impl LlmModelSelectionConfig {
    fn is_empty(&self) -> bool {
        let route_empty = self.route.as_ref().is_none_or(|route| {
            route.route_id.is_none()
                && route.label.is_none()
                && route.base_url.is_none()
                && route.api_key_env.is_none()
                && route.api_type.is_none()
        });

        self.family_id.is_none()
            && self.model_id.is_none()
            && route_empty
            && self.model_hints.is_none()
            && self.cost_per_m.is_none()
            && self.strong.is_none()
            // #2142: a selection that pins ONLY a context_window override is
            // meaningful — it must not be collapsed as "empty" and dropped.
            && self.context_window.is_none()
    }
}

/// Channel-specific credentials (tagged by type).
// The Matrix variant carries many user-mode fields; boxing a serde
// `tag`-serialized struct variant isn't worth the (de)serialization churn.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ChannelCredentials {
    Telegram {
        #[serde(default = "default_telegram_env")]
        token_env: String,
        #[serde(default)]
        allowed_senders: String,
    },
    Discord {
        #[serde(default = "default_discord_env")]
        token_env: String,
    },
    DingTalk {
        #[serde(default = "default_dingtalk_webhook_env")]
        webhook_url_env: String,
        #[serde(default = "default_dingtalk_secret_env")]
        secret_env: String,
        #[serde(default)]
        allowed_senders: String,
        #[serde(default)]
        webhook_port: Option<u16>,
    },
    Slack {
        #[serde(default = "default_slack_bot_env")]
        bot_token_env: String,
        #[serde(default = "default_slack_app_env")]
        app_token_env: String,
    },
    #[serde(rename = "whatsapp")]
    WhatsApp {
        #[serde(default = "default_whatsapp_url")]
        bridge_url: String,
    },
    Feishu {
        #[serde(default = "default_feishu_id_env")]
        app_id_env: String,
        #[serde(default = "default_feishu_secret_env")]
        app_secret_env: String,
        #[serde(default)]
        mode: String,
        #[serde(default)]
        region: String,
        #[serde(default)]
        webhook_port: Option<u16>,
        #[serde(default)]
        verification_token_env: String,
        #[serde(default)]
        encrypt_key_env: String,
    },
    Email {
        #[serde(default)]
        imap_host: String,
        #[serde(default = "default_imap_port")]
        imap_port: u16,
        #[serde(default)]
        smtp_host: String,
        #[serde(default = "default_smtp_port")]
        smtp_port: u16,
        #[serde(default = "default_email_user_env")]
        username_env: String,
        #[serde(default = "default_email_pass_env")]
        password_env: String,
    },
    Twilio {
        #[serde(default = "default_twilio_sid_env")]
        account_sid_env: String,
        #[serde(default = "default_twilio_token_env")]
        auth_token_env: String,
        #[serde(default)]
        from_number: String,
        #[serde(default = "default_twilio_webhook_port")]
        webhook_port: u16,
    },
    Api {
        #[serde(default = "default_api_port")]
        port: u16,
        #[serde(default)]
        auth_token: Option<String>,
    },
    #[serde(rename = "wecom-bot")]
    WeComBot {
        #[serde(default)]
        bot_id: String,
        #[serde(default = "default_wecom_bot_secret_env")]
        secret_env: String,
    },
    Matrix {
        #[serde(default)]
        homeserver: String,
        // Appservice-mode tokens. Optional so a user-mode entry (which has no
        // appservice registration) still deserializes; emptiness is enforced
        // downstream only for appservice mode.
        #[serde(default)]
        as_token: String,
        #[serde(default)]
        hs_token: String,
        #[serde(default)]
        server_name: String,
        #[serde(default = "default_matrix_sender_localpart")]
        sender_localpart: String,
        #[serde(default = "default_matrix_user_prefix")]
        user_prefix: String,
        #[serde(default = "default_matrix_port")]
        port: u16,
        #[serde(default)]
        allowed_senders: Vec<String>,
        /// Appservice-mode: outside a true 1:1 DM, bots only reply when
        /// explicitly addressed. Safe-by-default; set to `false` to let bots
        /// answer every message.
        #[serde(default = "crate::config::default_true")]
        mention_only: bool,
        /// Channel mode: "appservice" (default) or "user" (regular account login).
        #[serde(default)]
        mode: String,
        /// User-mode: Matrix user ID, e.g. "@bot:matrix.org".
        #[serde(default)]
        user_id: String,
        /// User-mode: access token (alternative to password login).
        #[serde(default)]
        access_token: String,
        /// User-mode: account password (alternative to access token).
        #[serde(default)]
        password: String,
        /// User-mode: device display name created at login.
        #[serde(default)]
        device_name: String,
        /// User-mode: room allowlist used when group_policy is "allowlist".
        #[serde(default)]
        rooms: Vec<String>,
        /// User-mode: invite auto-join policy: "off", "allowlist", or "always".
        #[serde(default = "default_matrix_auto_join")]
        auto_join: String,
        /// User-mode: invite allowlist used when auto_join is "allowlist".
        #[serde(default)]
        auto_join_allowlist: Vec<String>,
        /// User-mode: room/group policy: "open", "allowlist", or "disabled".
        #[serde(default = "default_matrix_group_policy")]
        group_policy: String,
        /// User-mode: require an explicit bot mention or slash command in allowed rooms.
        #[serde(default = "crate::config::default_true")]
        require_mention: bool,
    },
    #[serde(rename = "qq-bot")]
    QQBot {
        #[serde(default)]
        app_id: String,
        #[serde(default = "default_qq_bot_secret_env")]
        client_secret_env: String,
    },
    #[serde(rename = "wechat")]
    WeChat {
        #[serde(default = "default_wechat_token_env")]
        token_env: String,
        #[serde(default = "default_wechat_base_url")]
        base_url: String,
    },
    Line {
        #[serde(default = "default_line_secret_env")]
        channel_secret_env: String,
        #[serde(default = "default_line_token_env")]
        channel_access_token_env: String,
        #[serde(default)]
        allowed_senders: String,
        #[serde(default)]
        webhook_port: Option<u16>,
        #[serde(default)]
        require_mention: bool,
        #[serde(default)]
        bot_user_id: String,
    },
}

fn default_telegram_env() -> String {
    "TELEGRAM_BOT_TOKEN".into()
}
fn default_discord_env() -> String {
    "DISCORD_BOT_TOKEN".into()
}
fn default_dingtalk_webhook_env() -> String {
    "DINGTALK_BOT_WEBHOOK".into()
}
fn default_dingtalk_secret_env() -> String {
    "DINGTALK_BOT_SECRET".into()
}
fn default_slack_bot_env() -> String {
    "SLACK_BOT_TOKEN".into()
}
fn default_slack_app_env() -> String {
    "SLACK_APP_TOKEN".into()
}
fn default_whatsapp_url() -> String {
    "ws://localhost:3001".into()
}
fn default_feishu_id_env() -> String {
    "FEISHU_APP_ID".into()
}
fn default_feishu_secret_env() -> String {
    "FEISHU_APP_SECRET".into()
}
fn default_imap_port() -> u16 {
    993
}
fn default_smtp_port() -> u16 {
    465
}
fn default_email_user_env() -> String {
    "EMAIL_USERNAME".into()
}
fn default_email_pass_env() -> String {
    "EMAIL_PASSWORD".into()
}
fn default_twilio_sid_env() -> String {
    "TWILIO_ACCOUNT_SID".into()
}
fn default_twilio_token_env() -> String {
    "TWILIO_AUTH_TOKEN".into()
}
fn default_twilio_webhook_port() -> u16 {
    8090
}
fn default_api_port() -> u16 {
    8091
}
fn default_wecom_bot_secret_env() -> String {
    "WECOM_BOT_SECRET".into()
}
fn default_matrix_sender_localpart() -> String {
    "bot".into()
}
fn default_matrix_user_prefix() -> String {
    "bot_".into()
}
fn default_matrix_port() -> u16 {
    8009
}
fn default_matrix_auto_join() -> String {
    "off".into()
}
fn default_matrix_group_policy() -> String {
    "allowlist".into()
}
fn default_qq_bot_secret_env() -> String {
    "QQ_BOT_CLIENT_SECRET".into()
}
fn default_wechat_token_env() -> String {
    "WECHAT_BOT_TOKEN".into()
}
fn default_wechat_base_url() -> String {
    "https://ilinkai.weixin.qq.com".into()
}
fn default_line_secret_env() -> String {
    "LINE_CHANNEL_SECRET".into()
}
fn default_line_token_env() -> String {
    "LINE_CHANNEL_ACCESS_TOKEN".into()
}

/// Gateway-specific settings.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GatewaySettings {
    #[serde(default)]
    pub max_history: Option<usize>,
    #[serde(default)]
    pub max_iterations: Option<u32>,
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub max_concurrent_sessions: Option<usize>,
    #[serde(default)]
    pub browser_timeout_secs: Option<u64>,
    /// Default max output tokens per LLM call.
    /// Overrides the built-in default from model_limits.json.
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    /// Sampling temperature override for chat LLM calls. `None` keeps the
    /// built-in default (`0.0`/greedy). Primarily for local / OpenAI-compatible
    /// models, where forced greedy decoding causes repetition collapse. #2172.
    #[serde(default)]
    pub llm_temperature: Option<f32>,
    /// Extra sampler params (e.g. `repeat_penalty`) flattened into the request
    /// for OpenAI-compatible servers. `None` → nothing added. #2172.
    #[serde(default)]
    pub llm_sampling_params: Option<serde_json::Map<String, serde_json::Value>>,
    /// Per-profile watchdog override. `None` inherits the system monitor default.
    #[serde(default)]
    pub watchdog_enabled: Option<bool>,
    /// Per-profile alert override. `None` inherits the system monitor default.
    #[serde(default)]
    pub alerts_enabled: Option<bool>,
}

/// Manages profile storage as individual JSON files.
///
/// The store has two roots so multi-instance stdio can share one profile
/// REGISTRY while each instance owns private per-profile runtime state:
///
/// * `registry_dir` (`<registry_root>/profiles`) — where the `<id>.json`
///   registry files are read/written/listed, plus config-like siblings under
///   the registry parent (the `default-profile` pointer, platform-model
///   allowlist). Shared across instances.
/// * `data_profiles_dir` (`<data_root>/profiles`) — where [`resolve_data_dir`]
///   roots the per-profile `<id>/data` runtime tree when a profile carries no
///   explicit `data_dir` override. Per-instance.
///
/// [`resolve_data_dir`]: ProfileStore::resolve_data_dir
pub struct ProfileStore {
    /// Root for the `<id>.json` registry (shared/config-like).
    registry_dir: PathBuf,
    /// Root for the per-profile `<id>/data` runtime tree (per-instance).
    data_profiles_dir: PathBuf,
    /// Optional global base config layer loaded from
    /// `<registry_root>/profile-defaults.json`. When present, every profile
    /// inherits a curated subset of these fields as a base layer, with the
    /// profile's own config overriding — see [`ProfileStore::effective_config`].
    /// Absent file ⇒ `None` ⇒ zero behavior change (backward compatible).
    defaults: Option<ProfileConfig>,
}

impl ProfileStore {
    /// Open (or create) the profile store with a split registry / data root.
    ///
    /// * `registry_root` — the `<id>.json` registry lives under
    ///   `registry_root/profiles`.
    /// * `data_root` — the per-profile `<id>/data` fallback tree roots under
    ///   `data_root/profiles`.
    ///
    /// When `registry_root == data_root` this is byte-identical to the legacy
    /// single-root behavior; see [`ProfileStore::open_unified`].
    pub fn open(registry_root: &Path, data_root: &Path) -> Result<Self> {
        let registry_dir = registry_root.join("profiles");
        std::fs::create_dir_all(&registry_dir).wrap_err_with(|| {
            format!(
                "failed to create profiles registry dir: {}",
                registry_dir.display()
            )
        })?;
        let data_profiles_dir = data_root.join("profiles");
        std::fs::create_dir_all(&data_profiles_dir).wrap_err_with(|| {
            format!(
                "failed to create profiles data dir: {}",
                data_profiles_dir.display()
            )
        })?;
        let defaults = Self::load_defaults(registry_root);
        Ok(Self {
            registry_dir,
            data_profiles_dir,
            defaults,
        })
    }

    /// Read the optional `<registry_root>/profile-defaults.json` global base
    /// layer. A missing file yields `None`; a read or parse error is logged as
    /// a warning and also yields `None` — a malformed defaults file must never
    /// fail store open. Because [`ProfileConfig`] fields all carry
    /// `#[serde(default)]`, a partial file (e.g. `{"hooks":[...]}`) parses with
    /// the named fields set and every other field at its `Default`/`None`/empty
    /// value.
    fn load_defaults(registry_root: &Path) -> Option<ProfileConfig> {
        let path = registry_root.join("profile-defaults.json");
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "failed to read profile-defaults.json; ignoring global profile defaults"
                );
                return None;
            }
        };
        match serde_json::from_str::<ProfileConfig>(&content) {
            Ok(defaults) => Some(defaults),
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "malformed profile-defaults.json; ignoring global profile defaults"
                );
                None
            }
        }
    }

    /// Compute a profile's effective [`ProfileConfig`] by layering the store's
    /// global `profile-defaults.json` base UNDER the profile's own config.
    ///
    /// When no defaults file is present (`self.defaults == None`) this returns
    /// a byte-identical clone of `profile.config` — absent defaults ⇒ zero
    /// behavior change (backward compatible).
    ///
    /// See [`merge_profile_defaults`] for the field-by-field merge rules and
    /// the security-floor semantics applied to `sandbox` and `plugins`.
    pub fn effective_config(&self, profile: &UserProfile) -> ProfileConfig {
        match self.defaults.as_ref() {
            Some(defaults) => merge_profile_defaults(&profile.config, defaults),
            // Backward compatible: no defaults file ⇒ exact profile config.
            None => profile.config.clone(),
        }
    }

    /// Resolve a profile into the fully-inherited config a consumer should run
    /// with: parent / sub-account inheritance ([`resolve_effective_profile`])
    /// THEN the store's global `profile-defaults.json` base
    /// ([`effective_config`](Self::effective_config)) applied on top.
    ///
    /// This is the SINGLE source of a profile's runtime config — every place
    /// that reads a profile's sandbox / hooks / approvals / memory / plugin
    /// signing at runtime (serve's per-profile loop, the gateway, routed
    /// child bots) must resolve through here so both inheritance layers apply
    /// uniformly. Reading `profile.config` raw silently drops one or both
    /// layers.
    ///
    /// Infallible by design: a broken parent link is logged and the profile is
    /// used without parent inheritance, but the global-defaults layer is still
    /// applied (fail-safe — a missing parent must not drop the operator's base
    /// hooks / sandbox restrictions / signing floor).
    pub fn resolve_runtime_profile(&self, profile: &UserProfile) -> UserProfile {
        let mut resolved = match resolve_effective_profile(self, profile) {
            Ok(resolved) => resolved,
            Err(error) => {
                tracing::warn!(
                    profile_id = %profile.id,
                    %error,
                    "failed to resolve parent inheritance; applying global defaults to the \
                     profile without parent inheritance",
                );
                profile.clone()
            }
        };
        resolved.config = self.effective_config(&resolved);
        resolved
    }

    /// Open (or create) the profile store with a single unified root — the
    /// registry and per-profile data both live under `data_dir/profiles/`.
    ///
    /// This preserves the pre-split behavior exactly (registry == data) and is
    /// the right entry point for every caller that is not multi-instance-aware.
    pub fn open_unified(data_dir: &Path) -> Result<Self> {
        Self::open(data_dir, data_dir)
    }

    /// List all profiles sorted by name.
    pub fn list(&self) -> Result<Vec<UserProfile>> {
        let mut profiles = Vec::new();
        let entries = match std::fs::read_dir(&self.registry_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(profiles),
            Err(e) => return Err(e).wrap_err("failed to read profiles directory"),
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                match std::fs::read_to_string(&path) {
                    Ok(content) => match serde_json::from_str::<UserProfile>(&content) {
                        Ok(mut profile) => {
                            // Quarantine legacy records that predate the
                            // channel-name reservation (codex #1613 r5):
                            // a profile id equal to a channel produces
                            // ambiguous session keys, so it must not
                            // handle sessions. Skipping (same precedent
                            // as unparsable JSON above) keeps one legacy
                            // record from bricking the deployment; the
                            // warning tells the operator to rename it.
                            if octos_core::is_reserved_channel_name(&profile.id) {
                                tracing::warn!(
                                    path = %path.display(),
                                    id = %profile.id,
                                    "skipping profile whose id is a reserved channel name; \
                                     rename the profile (its session keys are ambiguous)"
                                );
                                continue;
                            }
                            profile.config.normalize_llm_contract();
                            profiles.push(profile);
                        }
                        Err(e) => {
                            tracing::warn!(path = %path.display(), error = %e, "skipping invalid profile");
                        }
                    },
                    Err(e) => {
                        tracing::warn!(path = %path.display(), error = %e, "failed to read profile");
                    }
                }
            }
        }
        profiles.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(profiles)
    }

    /// Get a single profile by ID.
    pub fn get(&self, id: &str) -> Result<Option<UserProfile>> {
        let path = self.profile_path(id);
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&path)
            .wrap_err_with(|| format!("failed to read profile: {id}"))?;
        let mut profile: UserProfile = serde_json::from_str(&content)
            .wrap_err_with(|| format!("failed to parse profile: {id}"))?;
        // Fail fast on legacy records that predate the channel-name
        // reservation (codex #1613 r5): a channel-named profile
        // produces ambiguous session keys (`api:telegram:123` parses as
        // a bare channel key), so it must not handle sessions. The
        // error names the fix instead of letting the record limp along.
        if octos_core::is_reserved_channel_name(&profile.id) {
            bail!(
                "profile '{}' uses a reserved channel name as its id and cannot be loaded; \
                 rename the profile file and its `id` field (e.g. `{}-bot`) — channel-named \
                 profiles produce ambiguous session keys",
                profile.id,
                profile.id
            );
        }
        profile.config.normalize_llm_contract();
        Ok(Some(profile))
    }

    /// Save a profile (create or update). Also initializes the data directory.
    pub fn save(&self, profile: &UserProfile) -> Result<()> {
        let mut normalized = profile.clone();
        normalized.config.normalize_llm_contract();

        validate_profile_id(&normalized.id)?;
        if let Some(slug) = normalized.public_subdomain.as_deref() {
            validate_public_subdomain(slug)?;
            self.ensure_public_subdomain_available(slug, Some(&normalized.id))?;
        }

        // Initialize data directory structure
        let data_dir = self.resolve_data_dir(&normalized);
        for sub in ["memory", "sessions", "research", "skills", "history"] {
            std::fs::create_dir_all(data_dir.join(sub)).ok();
        }

        let path = self.profile_path(&normalized.id);
        let mut serialized =
            serde_json::to_value(&normalized).wrap_err("failed to serialize profile")?;
        preserve_local_owner_metadata(&path, &mut serialized);
        let content =
            serde_json::to_string_pretty(&serialized).wrap_err("failed to serialize profile")?;

        // Atomic write: write to temp file, then rename to avoid partial writes
        // if the process is interrupted or concurrent saves race.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &content)
            .wrap_err_with(|| format!("failed to write temp profile: {}", tmp.display()))?;
        std::fs::rename(&tmp, &path)
            .wrap_err_with(|| format!("failed to rename profile: {}", path.display()))?;

        // Restrict file permissions to owner-only (mode 0600)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            if let Err(e) = std::fs::set_permissions(&path, perms) {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "failed to set restrictive permissions on profile file"
                );
            }
        }

        Ok(())
    }

    /// Save a profile, merging masked/empty secret values with the existing profile.
    ///
    /// For each env var: if the incoming value is masked (`***`), the keychain
    /// display indicator, or empty, the existing saved value is preserved.
    /// This prevents the masked values returned by GET from overwriting
    /// real secrets or keychain markers.
    pub fn save_with_merge(&self, profile: &mut UserProfile) -> Result<()> {
        if let Some(existing) = self.get(&profile.id)? {
            for (key, new_val) in profile.config.env_vars.iter_mut() {
                let is_masked = is_display_secret_value(new_val) || new_val.is_empty();
                // Never overwrite the real stored value with a display artifact,
                // but DO allow explicit "keychain:" marker (it's the real value).
                if is_masked && new_val != crate::auth::KEYCHAIN_MARKER {
                    if let Some(old_val) = existing.config.env_vars.get(key) {
                        *new_val = old_val.clone();
                    }
                }
            }
            for (idx, new_channel) in profile.config.channels.iter_mut().enumerate() {
                let old_channel = existing
                    .config
                    .channels
                    .iter()
                    .find(|old_channel| channel_secret_identity_matches(new_channel, old_channel))
                    .or_else(|| {
                        existing.config.channels.get(idx).filter(|old_channel| {
                            same_secret_channel_variant(new_channel, old_channel)
                        })
                    });
                if let Some(old_channel) = old_channel {
                    restore_masked_channel_secrets(new_channel, old_channel);
                }
            }
            // Same contract as channels: a client that GETs a masked profile and
            // PUTs it back must not overwrite the stored secret with `ab***xyz`.
            if let (Some(new_email), Some(old_email)) =
                (&mut profile.config.email, &existing.config.email)
            {
                restore_masked_optional_secret(&mut new_email.password, &old_email.password);
                restore_masked_optional_secret(
                    &mut new_email.feishu_app_secret,
                    &old_email.feishu_app_secret,
                );
            }
            if let (Some(new_smart_home), Some(old_smart_home)) =
                (&mut profile.config.smart_home, &existing.config.smart_home)
            {
                restore_masked_optional_secret(&mut new_smart_home.token, &old_smart_home.token);
            }
        }
        self.save(profile)
    }

    /// Delete a profile by ID.
    pub fn delete(&self, id: &str) -> Result<bool> {
        let path = self.profile_path(id);
        if !path.exists() {
            return Ok(false);
        }
        std::fs::remove_file(&path).wrap_err_with(|| format!("failed to delete profile: {id}"))?;
        Ok(true)
    }

    /// Resolve the data directory for a profile.
    pub fn resolve_data_dir(&self, profile: &UserProfile) -> PathBuf {
        if let Some(ref dir) = profile.data_dir {
            PathBuf::from(dir)
        } else {
            self.data_profiles_dir.join(&profile.id).join("data")
        }
    }

    pub(crate) fn profile_path(&self, id: &str) -> PathBuf {
        self.registry_dir.join(format!("{id}.json"))
    }

    /// Registration-id reservation policy (codex #1613 r6/r8), wired
    /// into `AuthManager::with_id_taken_probe` by the serve bootstrap:
    /// `(candidate, authorized) -> taken`.
    ///
    /// - No profile file → free for anyone.
    /// - File exists, ANONYMOUS registration → taken: a generated id
    ///   must never claim an admin-created-but-unclaimed profile (r6).
    /// - File exists, AUTHORIZED claim (allowlist provenance for this
    ///   exact derived id) → claimable ONLY when the record loads
    ///   cleanly. An unloadable file (corrupt json, quarantined
    ///   channel-name id) stays reserved: the verify path's
    ///   auto-create treats a `get` error as "no profile" and would
    ///   OVERWRITE the file with a default profile (r8 P2).
    // The only non-test caller (the serve bootstrap) is api-gated; the
    // policy itself stays unconditional next to the store it guards.
    #[cfg_attr(not(feature = "api"), allow(dead_code))]
    pub(crate) fn id_reserved_for_registration(&self, id: &str, authorized: bool) -> bool {
        if !self.profile_path(id).exists() {
            return false;
        }
        if !authorized {
            return true;
        }
        !matches!(self.get(id), Ok(Some(_)))
    }

    /// Return the parent directory of the registry profiles dir (i.e. the octos
    /// home dir). This is the REGISTRY root — the config-like siblings that live
    /// beside the profiles tree (the `default-profile` pointer, the platform-
    /// model allowlist, and the `--octos-home` a spawned gateway opens its own
    /// `ProfileStore` from) all resolve from here, so they stay shared across
    /// per-instance runtime dirs. With a unified store this is the data dir,
    /// exactly as before.
    pub fn octos_home_dir(&self) -> &Path {
        self.registry_dir.parent().unwrap_or(&self.registry_dir)
    }

    /// Path to the persisted global default-profile pointer
    /// (`<octos-home>/default-profile`).
    fn default_profile_pointer_path(&self) -> PathBuf {
        self.octos_home_dir().join("default-profile")
    }

    /// The explicitly-chosen global default profile id, if one was set with
    /// [`Self::set_default_profile`] and the pointer file is readable. Trimmed;
    /// an empty or missing pointer yields `None`. The launch resolver
    /// (`launch/resolve`) uses this as the top default for a bare launch that
    /// has no folder-sticky profile.
    pub fn default_profile(&self) -> Option<String> {
        let raw = std::fs::read_to_string(self.default_profile_pointer_path()).ok()?;
        let trimmed = raw.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    }

    /// Persist `id` as the machine's global default profile, replacing any prior
    /// pointer. Atomic write-then-rename within the octos home dir so a crash
    /// mid-write cannot leave a torn pointer.
    pub fn set_default_profile(&self, id: &str) -> Result<()> {
        let path = self.default_profile_pointer_path();
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, id.trim().as_bytes()).wrap_err_with(|| {
            format!("failed to write default-profile pointer: {}", tmp.display())
        })?;
        std::fs::rename(&tmp, &path).wrap_err_with(|| {
            format!(
                "failed to install default-profile pointer: {}",
                path.display()
            )
        })?;
        Ok(())
    }

    /// List sub-accounts for a given parent profile.
    ///
    /// NOTE(#148): This performs an O(N) scan over all profiles and filters by parent_id.
    /// For small deployments (<100 profiles) this is fine. If profile counts grow large,
    /// consider adding a secondary index (e.g. a parent_id -> Vec<sub_id> mapping) or
    /// storing sub-accounts in a subdirectory per parent.
    pub fn list_sub_accounts(&self, parent_id: &str) -> Result<Vec<UserProfile>> {
        let all = self.list()?;
        Ok(all
            .into_iter()
            .filter(|p| p.parent_id.as_deref() == Some(parent_id))
            .collect())
    }

    /// Resolve a public host slug to an internal profile ID.
    ///
    /// Host routing is authoritative on `public_subdomain`. For top-level
    /// profiles only, we allow falling back to the immutable internal ID when
    /// no explicit public slug has been configured.
    pub fn resolve_routable_profile_id(&self, candidate: &str) -> Result<Option<String>> {
        if let Some(profile) = self.get_by_public_subdomain(candidate)? {
            return Ok(Some(profile.id));
        }

        let Some(profile) = self.get(candidate)? else {
            return Ok(None);
        };

        if profile.parent_id.is_none() && profile.public_subdomain.is_none() {
            return Ok(Some(profile.id));
        }

        Ok(None)
    }

    pub fn get_by_public_subdomain(&self, slug: &str) -> Result<Option<UserProfile>> {
        let normalized = slug.trim();
        if normalized.is_empty() {
            return Ok(None);
        }

        Ok(self
            .list()?
            .into_iter()
            .find(|profile| profile.public_subdomain.as_deref() == Some(normalized)))
    }

    pub fn ensure_public_subdomain_available(
        &self,
        slug: &str,
        except_profile_id: Option<&str>,
    ) -> Result<()> {
        let normalized = slug.trim();
        validate_public_subdomain(normalized)?;

        for profile in self.list()? {
            if except_profile_id == Some(profile.id.as_str()) {
                continue;
            }
            if profile.id == normalized || profile.public_subdomain.as_deref() == Some(normalized) {
                bail!("public subdomain '{normalized}' is already in use");
            }
        }
        Ok(())
    }

    /// Create a sub-account under a parent profile.
    ///
    /// The sub-account inherits the parent's LLM contract at runtime.
    /// It has its own channels, gateway settings, and data directory.
    pub fn create_sub_account(
        &self,
        parent_id: &str,
        sub_account_id: &str,
        public_subdomain: &str,
        sub_name: &str,
        channels: Vec<ChannelCredentials>,
        gateway: GatewaySettings,
    ) -> Result<UserProfile> {
        // Verify parent exists
        let parent = self
            .get(parent_id)?
            .ok_or_else(|| eyre::eyre!("parent profile '{parent_id}' not found"))?;
        if parent.parent_id.is_some() {
            bail!("sub-account '{parent_id}' cannot own sub-accounts");
        }

        let existing_subs = self.list_sub_accounts(parent_id)?;
        if existing_subs.len() >= MAX_SUB_ACCOUNTS_PER_PARENT {
            bail!(
                "profile '{parent_id}' already has the maximum of {MAX_SUB_ACCOUNTS_PER_PARENT} sub-accounts"
            );
        }

        let sub_id = format!("{parent_id}--{}", sub_account_id.trim());
        validate_profile_id(&sub_id)?;
        self.ensure_public_subdomain_available(public_subdomain, None)?;

        if self.get(&sub_id)?.is_some() {
            bail!("sub-account '{sub_id}' already exists");
        }

        let now = Utc::now();
        let profile = UserProfile {
            id: sub_id,
            name: sub_name.to_string(),
            public_subdomain: Some(public_subdomain.trim().to_string()),
            enabled: false,
            data_dir: None,
            parent_id: Some(parent_id.to_string()),
            config: ProfileConfig {
                llm: None,
                // Sub-account's own settings
                channels,
                gateway,
                ..Default::default()
            },
            created_at: now,
            updated_at: now,
        };

        self.save(&profile)?;
        Ok(profile)
    }
}

fn preserve_local_owner_metadata(path: &Path, serialized: &mut serde_json::Value) {
    let Some(object) = serialized.as_object_mut() else {
        return;
    };
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(serde_json::Value::Object(existing)) =
        serde_json::from_str::<serde_json::Value>(&content)
    else {
        return;
    };
    for key in ["username", "email"] {
        if object.contains_key(key) {
            continue;
        }
        if let Some(value) = existing.get(key).filter(|value| value.is_string()) {
            object.insert(key.to_owned(), value.clone());
        }
    }
}

/// Layer a global `profile-defaults.json` base UNDER a profile's own
/// [`ProfileConfig`], returning the effective config a consumer should run
/// with. `base` is the profile's own config; `defaults` is the global base.
///
/// # Inherited fields (defaults act as a base layer)
/// * `hooks` — additive: `defaults.hooks` followed by the profile's hooks
///   (defaults first, order preserved).
/// * `env_vars` — merged: defaults form the base, the profile's keys win on
///   any duplicate key.
///
///   TRUST BOUNDARY: inherited `env_vars` intentionally SHARE the operator's
///   credentials across every profile — a `profile-defaults.json` API key is
///   visible to all profiles by design. This is acceptable for a
///   single-operator deployment; it is NOT tenant isolation. Do not place one
///   tenant's secrets in the defaults expecting another tenant not to receive
///   them; per-tenant credentials belong on each profile's own `env_vars`.
/// * `plugins`, `sandbox` — presence-aware, field-by-field (see below).
/// * `memory`, `approval_policy`, `cost_budget` — `Option` fallback: the
///   profile's value wins; a `None` falls back to the defaults'.
///
/// # Presence-aware merge for `sandbox` / `plugins`
/// A field inherits from `defaults` ONLY when the profile left it at the
/// type's `Default` value — our proxy for "omitted on disk". (The merge runs
/// on the typed, already-loaded profile, so a field the profile omitted is
/// indistinguishable from one it explicitly set to the default.) This means a
/// profile that sets ONE sandbox field (e.g. `read_allow_paths`) still
/// inherits every OTHER default sandbox restriction, instead of the whole
/// struct reverting to type defaults the moment any single field is set — the
/// bug this replaces.
///
/// # Security floors (a profile may tighten, never loosen)
/// Applied AFTER the merge, and ONLY for fields whose *restrictive* value
/// differs from the type `Default` — so a `profile-defaults.json` that merely
/// round-trips the type defaults never spuriously tightens a profile:
/// * `plugins.require_signed` (restrictive `true` ≠ default `false`): logical
///   OR. If the defaults require signing, a profile can NOT turn it off.
/// * `sandbox.workspace_write` (restrictive `false` ≠ default `true`): logical
///   AND. A defaults `workspace_write: false` (read-only workspace) can NOT be
///   lifted by a profile.
/// * `sandbox.read_allow_paths` (restrictive non-empty ≠ default empty): when
///   the defaults restrict reads, a profile may only pick a SUBSET (paths at or
///   under an operator-approved root); paths outside every root are dropped and
///   an emptied/allow-all list is clamped back to the operator's set.
///
/// `sandbox.allow_network` / `sandbox.enabled` have a restrictive value that
/// EQUALS the type default, so a standalone floor would fire on every
/// round-tripped defaults file. For those the presence-aware merge alone is the
/// guard: a profile that sets an *unrelated* field can no longer silently drop
/// a default `allow_network: false` / `enabled: true` (the omitted field is
/// inherited). An *explicit* profile setting is honored — profiles are
/// operator-authored (single-operator trust model), not tenant-uploaded.
///
/// Every field NOT listed above is taken verbatim from the profile, so any
/// field added to [`ProfileConfig`] in the future is profile-only by default —
/// fail-safe against leaking a new identity/channel field through the shared
/// defaults.
pub(crate) fn merge_profile_defaults(
    base: &ProfileConfig,
    defaults: &ProfileConfig,
) -> ProfileConfig {
    let mut effective = base.clone();

    // hooks: defaults first, then the profile's own (additive, ordered).
    let mut hooks = defaults.hooks.clone();
    hooks.extend(std::mem::take(&mut effective.hooks));
    effective.hooks = hooks;

    // env_vars: defaults as the base; profile keys override per key.
    // Trust boundary: see the fn doc — inherited env_vars share operator
    // credentials across all profiles (single-operator, NOT tenant isolation).
    let mut env_vars = defaults.env_vars.clone();
    env_vars.extend(std::mem::take(&mut effective.env_vars));
    effective.env_vars = env_vars;

    // plugins / sandbox: presence-aware field merge + security floors.
    effective.plugins = merge_plugins_defaults(&base.plugins, &defaults.plugins);
    effective.sandbox = merge_sandbox_defaults(&base.sandbox, &defaults.sandbox);

    // skills: inherited selection layer (union of rules, last-wins per id).
    effective.skills = merge_skills(&defaults.skills, &base.skills);

    // memory / approval_policy / cost_budget: profile wins, else defaults.
    if effective.memory.is_none() {
        effective.memory = defaults.memory.clone();
    }
    if effective.approval_policy.is_none() {
        effective.approval_policy = defaults.approval_policy.clone();
    }
    if effective.cost_budget.is_none() {
        effective.cost_budget = defaults.cost_budget.clone();
    }
    // #2168: tool_policy inherits like its sibling Option fields — the
    // profile's own wins, else the operator default applies.
    if effective.tool_policy.is_none() {
        effective.tool_policy = defaults.tool_policy.clone();
    }

    effective
}

/// Merge the inherited skill-selection layer.
///
/// This is ordinary selection inheritance (NOT a security floor): a profile
/// rule for the same id fully REPLACES the defaults' rule (last-wins per id),
/// so a profile may re-enable a skill the defaults disabled. The result's id
/// set is the union of both layers' ids.
///
/// - `rules`: defaults' rules first (order preserved); each profile rule either
///   replaces the defaults' rule for the same id in place or is appended.
/// - `mode`: `profile.mode.or(defaults.mode)` — the profile's explicit mode
///   wins, else the defaults' mode is inherited, else `None` (⇒ AllDiscovered).
/// - `None` + `None` ⇒ `None` (byte-identical to pre-skill-layering configs).
pub(crate) fn merge_skills(
    defaults: &Option<ProfileSkillsConfig>,
    profile: &Option<ProfileSkillsConfig>,
) -> Option<ProfileSkillsConfig> {
    match (defaults, profile) {
        (None, None) => None,
        (Some(defaults), None) => Some(defaults.clone()),
        (None, Some(profile)) => Some(profile.clone()),
        (Some(defaults), Some(profile)) => {
            let mut rules = defaults.rules.clone();
            for profile_rule in &profile.rules {
                if let Some(existing) = rules.iter_mut().find(|rule| rule.id == profile_rule.id) {
                    *existing = profile_rule.clone();
                } else {
                    rules.push(profile_rule.clone());
                }
            }
            Some(ProfileSkillsConfig {
                mode: profile.mode.or(defaults.mode),
                rules,
            })
        }
    }
}

/// Presence-aware merge of `plugins` with a security floor on `require_signed`.
///
/// `require_signed` is a one-way ratchet: a profile can turn signing ON, and a
/// `defaults.require_signed = true` forces it on even when the profile omits or
/// explicitly disables it (logical OR). The merge (profile-if-set) and the
/// floor (a required signing can't be disabled) collapse to a single OR, so a
/// profile can only tighten the operator's signing policy, never loosen it.
fn merge_plugins_defaults(
    base: &crate::config::PluginsConfig,
    defaults: &crate::config::PluginsConfig,
) -> crate::config::PluginsConfig {
    crate::config::PluginsConfig {
        require_signed: base.require_signed || defaults.require_signed,
    }
}

/// Presence-aware, field-by-field merge of `sandbox` with security floors.
///
/// See [`merge_profile_defaults`] for the full semantics. In short: a field is
/// inherited from `defaults` when the profile left it at the type default
/// (proxy for "omitted"); then `workspace_write` (logical AND) and
/// `read_allow_paths` (subset clamp) enforce a floor a profile can tighten but
/// not loosen.
fn merge_sandbox_defaults(
    base: &octos_agent::SandboxConfig,
    defaults: &octos_agent::SandboxConfig,
) -> octos_agent::SandboxConfig {
    let type_default = octos_agent::SandboxConfig::default();
    let mut eff = base.clone();

    // Presence-aware fill: inherit a defaults field only where the profile left
    // it at the type default. This keeps every OTHER default restriction when a
    // profile sets just one sandbox field.
    if base.enabled == type_default.enabled {
        eff.enabled = defaults.enabled;
    }
    if base.mode == type_default.mode {
        eff.mode = defaults.mode.clone();
    }
    if base.allow_network == type_default.allow_network {
        eff.allow_network = defaults.allow_network;
    }
    if base.workspace_write == type_default.workspace_write {
        eff.workspace_write = defaults.workspace_write;
    }
    if base.docker == type_default.docker {
        eff.docker = defaults.docker.clone();
    }
    if base.read_allow_paths == type_default.read_allow_paths {
        eff.read_allow_paths = defaults.read_allow_paths.clone();
    }
    if base.profile_name == type_default.profile_name {
        eff.profile_name = defaults.profile_name.clone();
    }

    // --- Security floors: a profile may tighten, never loosen. Applied only
    // where the restrictive value differs from the type default, so a defaults
    // file that round-trips the type defaults never spuriously tightens.

    // workspace_write: restrictive `false` (type default `true`). A read-only
    // workspace mandated by the defaults can never be lifted by a profile.
    eff.workspace_write = eff.workspace_write && defaults.workspace_write;

    // read_allow_paths: a non-empty defaults list restricts reads to those
    // roots. A profile may only narrow to a subset (paths at/under an operator
    // root); paths outside every root are dropped, and emptying the list back
    // to allow-all is clamped to the operator's set.
    if !defaults.read_allow_paths.is_empty() {
        let clamped: Vec<String> = eff
            .read_allow_paths
            .iter()
            .filter(|path| sandbox_path_within_any(path, &defaults.read_allow_paths))
            .cloned()
            .collect();
        eff.read_allow_paths = if clamped.is_empty() {
            defaults.read_allow_paths.clone()
        } else {
            clamped
        };
    }

    eff
}

/// Lexically normalize an absolute `/`-rooted path, collapsing `.`/`..`
/// segments WITHOUT touching the filesystem (the read roots may not exist yet).
/// Returns `None` for a non-absolute path or one whose `..` segments climb above
/// the root — such a path can never be a valid subset of an operator root, so
/// the clamp drops it. This is what closes the `..` traversal that a bare
/// `strip_prefix` check would wave through (e.g. `/srv/data/../../etc` → `/etc`).
fn normalize_abs_lexical(path: &str) -> Option<String> {
    if !path.starts_with('/') {
        return None;
    }
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                // Escapes above the root — reject the whole path.
                out.pop()?;
            }
            seg => out.push(seg),
        }
    }
    Some(format!("/{}", out.join("/")))
}

/// True when `path` is exactly one of `roots` or nested beneath one of them.
/// Enforces the `read_allow_paths` floor: a profile may only keep read roots
/// that fall within an operator-approved root. Both sides are lexically
/// normalized first so a profile CANNOT use `..` (or a non-absolute path) to
/// escape an operator root and still pass the subset check.
fn sandbox_path_within_any(path: &str, roots: &[String]) -> bool {
    let Some(path) = normalize_abs_lexical(path) else {
        return false;
    };
    roots.iter().any(|root| {
        let Some(root) = normalize_abs_lexical(root) else {
            return false;
        };
        // A `/` root allows all absolute paths; otherwise require an exact
        // match or a `<root>/…` descendant (not a mere string prefix, so
        // `/srv/database` is NOT within `/srv/data`).
        root == "/"
            || path == root
            || path
                .strip_prefix(&root)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// Resolve the effective config for a profile. If it's a sub-account,
/// LLM provider fields are inherited from the parent.
pub fn resolve_effective_profile(
    store: &ProfileStore,
    profile: &UserProfile,
) -> Result<UserProfile> {
    let parent_id = match &profile.parent_id {
        Some(id) => id,
        None => return Ok(profile.clone()),
    };

    let parent = store
        .get(parent_id)?
        .ok_or_else(|| eyre::eyre!("parent profile '{parent_id}' not found"))?;

    let mut effective = profile.clone();
    let pc = &parent.config;
    let ec = &mut effective.config;

    // Inherit the LLM contract from parent.
    ec.llm = pc.llm.clone();
    if ec.review.is_none() {
        ec.review = pc.review.clone();
    }
    if ec.search.is_none() {
        ec.search = pc.search.clone();
    }
    if ec.deep_crawl.is_none() {
        ec.deep_crawl = pc.deep_crawl.clone();
    }
    if ec.apps.is_none() {
        ec.apps = pc.apps.clone();
    }

    // Inherit email config if sub-account doesn't have its own
    if ec.email.is_none() {
        ec.email = pc.email.clone();
    }
    // #2168: inherit the parent's tool_policy when the sub-account has none.
    if ec.tool_policy.is_none() {
        ec.tool_policy = pc.tool_policy.clone();
    }

    // Merge env_vars: parent as base, sub-account overrides win
    let mut merged_env = pc.env_vars.clone();
    merged_env.extend(ec.env_vars.clone());
    ec.env_vars = merged_env;

    Ok(effective)
}

fn validate_public_subdomain(slug: &str) -> Result<()> {
    // Only the SHAPE is shared with profile ids. The channel-name
    // reservation does NOT apply here: a public subdomain never
    // occupies the SessionKey profile segment (it resolves to the
    // owning profile's real id), so `slack`/`telegram`/`line` remain
    // valid slugs (codex #1613 r5 P2). The subdomain namespace keeps
    // its own reserved list below.
    validate_slug_shape(slug, "public subdomain")?;
    if matches!(slug, "www" | "app" | "admin" | "api" | "crew" | "octos") {
        bail!("public subdomain '{slug}' is reserved");
    }
    Ok(())
}

/// Return a copy of the profile with secret values in `env_vars` masked.
/// Shows the first 4 and last 3 characters for keys longer than 12 chars,
/// otherwise replaces the entire value with `***`.
/// Keychain-backed values show as a special indicator.
pub fn mask_secrets(profile: &UserProfile) -> UserProfile {
    let mut masked = profile.clone();
    for value in masked.config.env_vars.values_mut() {
        // Any keychain marker (bare or profile-scoped) shows the indicator,
        // never a mangled mask of the marker string itself.
        if crate::auth::keychain::is_marker(value) {
            *value = KEYCHAIN_DISPLAY.to_string();
        } else {
            *value = mask_value(value);
        }
    }
    for channel in &mut masked.config.channels {
        mask_channel_secrets(channel);
    }
    if let Some(email) = &mut masked.config.email {
        mask_email_secrets(email);
    }
    if let Some(smart_home) = &mut masked.config.smart_home {
        mask_smart_home_secrets(smart_home);
    }
    masked.config.normalize_llm_contract();
    masked
}

/// Mask the literal secret in `config.smart_home`.
///
/// `token` holds a real credential; `bridge_url` is not a secret (just a LAN
/// address the user needs to see/edit), and `token_env` holds only an env var
/// NAME, so both stay in the clear.
fn mask_smart_home_secrets(smart_home: &mut SmartHomeConfig) {
    if let Some(token) = &mut smart_home.token {
        *token = mask_value(token);
    }
}

/// Mask the literal secrets in `config.email`.
///
/// `password` and `feishu_app_secret` hold real credentials. Their `*_env`
/// twins hold only env var NAMES, so those stay in the clear — masking them
/// would hide which variable to set without protecting anything.
///
/// Until this existed `mask_secrets` covered `env_vars` and `channels` but not
/// `config.email`, so `GET /api/me` handed every authenticated caller their own
/// SMTP password and Feishu app secret in plaintext.
fn mask_email_secrets(email: &mut EmailSettings) {
    if let Some(password) = &mut email.password {
        *password = mask_value(password);
    }
    if let Some(secret) = &mut email.feishu_app_secret {
        *secret = mask_value(secret);
    }
}

/// Display string for keychain-backed values in API responses.
const KEYCHAIN_DISPLAY: &str = "\u{1f511} (keychain)";

pub(crate) fn is_display_secret_value(value: &str) -> bool {
    value.contains("***") || value.contains(KEYCHAIN_DISPLAY)
}

fn mask_value(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    if len > 12 {
        let prefix: String = chars[..4].iter().collect();
        let suffix: String = chars[len - 3..].iter().collect();
        format!("{prefix}***{suffix}")
    } else if len > 0 {
        "***".into()
    } else {
        String::new()
    }
}

fn mask_channel_secrets(channel: &mut ChannelCredentials) {
    match channel {
        ChannelCredentials::Api {
            auth_token: Some(token),
            ..
        } => {
            *token = mask_value(token);
        }
        ChannelCredentials::Matrix {
            as_token,
            hs_token,
            access_token,
            password,
            ..
        } => {
            *as_token = mask_value(as_token);
            *hs_token = mask_value(hs_token);
            *access_token = mask_value(access_token);
            *password = mask_value(password);
        }
        _ => {}
    }
}

fn restore_masked_channel_secrets(
    new_channel: &mut ChannelCredentials,
    old_channel: &ChannelCredentials,
) {
    match (new_channel, old_channel) {
        (
            ChannelCredentials::Api {
                auth_token: new_token,
                ..
            },
            ChannelCredentials::Api {
                auth_token: old_token,
                ..
            },
        ) => restore_masked_optional_secret(new_token, old_token),
        (
            ChannelCredentials::Matrix {
                as_token: new_as_token,
                hs_token: new_hs_token,
                access_token: new_access_token,
                password: new_password,
                ..
            },
            ChannelCredentials::Matrix {
                as_token: old_as_token,
                hs_token: old_hs_token,
                access_token: old_access_token,
                password: old_password,
                ..
            },
        ) => {
            restore_masked_secret(new_as_token, old_as_token);
            restore_masked_secret(new_hs_token, old_hs_token);
            restore_masked_secret(new_access_token, old_access_token);
            restore_masked_secret(new_password, old_password);
        }
        _ => {}
    }
}

fn same_secret_channel_variant(
    new_channel: &ChannelCredentials,
    old_channel: &ChannelCredentials,
) -> bool {
    matches!(
        (new_channel, old_channel),
        (
            ChannelCredentials::Api { .. },
            ChannelCredentials::Api { .. }
        ) | (
            ChannelCredentials::Matrix { .. },
            ChannelCredentials::Matrix { .. }
        )
    )
}

fn channel_secret_identity_matches(
    new_channel: &ChannelCredentials,
    old_channel: &ChannelCredentials,
) -> bool {
    match (new_channel, old_channel) {
        (
            ChannelCredentials::Api { port: new_port, .. },
            ChannelCredentials::Api { port: old_port, .. },
        ) => new_port == old_port,
        (
            ChannelCredentials::Matrix {
                homeserver: new_homeserver,
                mode: new_mode,
                user_id: new_user_id,
                server_name: new_server_name,
                sender_localpart: new_sender_localpart,
                user_prefix: new_user_prefix,
                port: new_port,
                ..
            },
            ChannelCredentials::Matrix {
                homeserver: old_homeserver,
                mode: old_mode,
                user_id: old_user_id,
                server_name: old_server_name,
                sender_localpart: old_sender_localpart,
                user_prefix: old_user_prefix,
                port: old_port,
                ..
            },
        ) => {
            if !new_mode.eq_ignore_ascii_case(old_mode) {
                return false;
            }
            let same_homeserver = !new_homeserver.trim().is_empty()
                && new_homeserver.trim_end_matches('/') == old_homeserver.trim_end_matches('/');
            if new_mode.eq_ignore_ascii_case("user") {
                same_homeserver && !new_user_id.trim().is_empty() && new_user_id == old_user_id
            } else {
                same_homeserver
                    && new_port == old_port
                    && !new_server_name.trim().is_empty()
                    && new_server_name == old_server_name
                    && new_sender_localpart == old_sender_localpart
                    && new_user_prefix == old_user_prefix
            }
        }
        _ => false,
    }
}

fn restore_masked_secret(new_value: &mut String, old_value: &str) {
    if is_display_secret_value(new_value) {
        *new_value = old_value.to_string();
    }
}

fn restore_masked_optional_secret(new_value: &mut Option<String>, old_value: &Option<String>) {
    let should_restore = new_value
        .as_deref()
        .map(is_display_secret_value)
        .unwrap_or(false);
    if should_restore {
        *new_value = old_value.clone();
    }
}

/// Shared slug shape check for profile IDs and public subdomains:
/// 1-64 chars, lowercase alphanumerics and hyphens, no edge hyphens.
fn validate_slug_shape(value: &str, what: &str) -> Result<()> {
    if value.is_empty() || value.len() > 64 {
        bail!("{what} must be 1-64 characters");
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        bail!("{what} must contain only lowercase letters, digits, and hyphens");
    }
    if value.starts_with('-') || value.ends_with('-') {
        bail!("{what} must not start or end with a hyphen");
    }
    Ok(())
}

/// Validate a profile ID (slug format).
fn validate_profile_id(id: &str) -> Result<()> {
    validate_slug_shape(id, "profile ID")?;
    // Reserve session-key channel names: a profile id equal to a
    // channel (`api`, `slack`, `line`, …) makes the profiled key
    // `{id}:{channel}:{chat}` indistinguishable from a bare
    // `{channel}:{chat}` under `split_base_key`, mis-scoping the
    // session everywhere (profile_id/channel/chat_id) — and forking it
    // would persist a wrongly-scoped child (codex #1613 r4). This
    // reservation applies ONLY to profile ids — see
    // validate_public_subdomain (codex #1613 r5 P2).
    if octos_core::is_reserved_channel_name(id) {
        bail!("profile ID must not be a reserved channel name (e.g. api, slack, line)");
    }
    Ok(())
}

/// Build a `Config` in-memory from a `UserProfile`, without writing any file.
///
/// Used by `octos gateway --profile <path>` to load configuration directly
/// from the profile JSON (the single source of truth).
pub(crate) fn config_from_profile(
    profile: &UserProfile,
    bridge_url_override: Option<&str>,
    feishu_port_override: Option<u16>,
) -> Config {
    let mut normalized = profile.clone();
    normalized.config.normalize_llm_contract();
    let profile = &normalized;
    let primary = profile
        .config
        .llm
        .as_ref()
        .and_then(|llm| llm.primary.as_ref());

    let channels: Vec<ChannelEntry> = profile
        .config
        .channels
        .iter()
        .map(|ch| {
            let mut entry = channel_to_entry(ch);
            // Override WhatsApp bridge_url if managed
            if let ChannelCredentials::WhatsApp { .. } = ch {
                if let Some(url) = bridge_url_override {
                    entry["settings"]["bridge_url"] = serde_json::json!(url);
                }
            }
            // Override webhook_port if auto-assigned (Feishu webhook / LINE)
            if matches!(
                ch,
                ChannelCredentials::Feishu { .. }
                    | ChannelCredentials::Line { .. }
                    | ChannelCredentials::DingTalk { .. }
            ) {
                if let Some(port) = feishu_port_override {
                    entry["settings"]["webhook_port"] = serde_json::json!(port);
                }
            }
            // Convert serde_json::Value → ChannelEntry
            serde_json::from_value(entry).expect("channel_to_entry produces valid ChannelEntry")
        })
        .collect();

    let fallback_models: Vec<FallbackModel> = profile
        .config
        .llm
        .as_ref()
        .map(|llm| llm.fallbacks.iter())
        .into_iter()
        .flatten()
        .map(|fb| FallbackModel {
            provider: fb.family_id.clone().unwrap_or_default(),
            model: fb.model_id.clone(),
            base_url: fb.route.as_ref().and_then(|route| route.base_url.clone()),
            api_key_env: fb
                .route
                .as_ref()
                .and_then(|route| route.api_key_env.clone()),
            model_hints: fb.model_hints.clone(),
            api_type: fb.route.as_ref().and_then(|route| route.api_type.clone()),
            cost_per_m: fb.cost_per_m,
            strong: fb.strong.unwrap_or_else(crate::config::default_true),
            // #2142: per-fallback operator override of the effective window.
            context_window: fb.context_window,
        })
        .collect();

    Config {
        provider: primary.and_then(|selection| selection.family_id.clone()),
        model: primary.and_then(|selection| selection.model_id.clone()),
        // #2142: operator override of the primary's effective context window.
        context_window: primary.and_then(|selection| selection.context_window),
        base_url: primary.and_then(|selection| {
            selection
                .route
                .as_ref()
                .and_then(|route| route.base_url.clone())
        }),
        api_key_env: primary.and_then(|selection| {
            selection
                .route
                .as_ref()
                .and_then(|route| route.api_key_env.clone())
        }),
        env_vars: profile.config.env_vars.clone(),
        // Internal-only flag (octos-ffi opt-out); profiles always use the
        // default auth-store resolution order.
        bypass_auth_store: false,
        api_type: primary.and_then(|selection| {
            selection
                .route
                .as_ref()
                .and_then(|route| route.api_type.clone())
        }),
        max_iterations: profile.config.gateway.max_iterations,
        gateway: Some(GatewayConfig {
            channels,
            max_history: profile.config.gateway.max_history.unwrap_or(50),
            system_prompt: profile.config.gateway.system_prompt.clone(),
            max_concurrent_sessions: profile.config.gateway.max_concurrent_sessions.unwrap_or(10),
            browser_timeout_secs: profile.config.gateway.browser_timeout_secs,
            max_output_tokens: profile.config.gateway.max_output_tokens,
            // #2172: surface the profile's temperature override to serve /
            // octoscode sessions (which run via a profile), so a local model
            // can escape forced greedy decoding.
            llm_temperature: profile.config.gateway.llm_temperature,
            // #2172: same for the sampler passthrough (repeat_penalty, …).
            llm_sampling_params: profile.config.gateway.llm_sampling_params.clone(),
            ..Default::default()
        }),
        fallback_models,
        // Fields not configured through profiles — use defaults
        version: None,
        model_hints: primary.and_then(|selection| selection.model_hints.clone()),
        // OLP #29 S2b: thread the profile's [[mcp_servers]] into the runtime
        // config so the gateway actually spawns them and registers their
        // tools (was hard-zeroed: profile-level MCP config never took effect).
        mcp_servers: profile.config.mcp_servers.clone(),
        sandbox: profile.config.sandbox.clone(),
        // (serve-side wiring lands with the UI/RPC follow-up).
        // #1768: thread the profile's snapshot opt-in so serve sessions
        // honor it (parity with format_after_edit).
        snapshots: profile.config.snapshots.clone(),
        // #2168: carry the profile's tool policy so a serve / UserProfile
        // session can slim its roster (the serve path already applies this).
        tool_policy: profile.config.tool_policy.clone(),
        tool_policy_by_provider: Default::default(),
        embedding: None,
        memory: profile.config.memory.clone(),
        hooks: profile.config.hooks.clone(),
        approval_policy: profile.config.approval_policy.clone(),
        context_filter: vec![],
        sub_providers: profile.config.sub_providers.clone(),
        email: profile
            .config
            .email
            .as_ref()
            .map(|e| crate::config::EmailConfig {
                provider: e.provider.clone(),
                smtp_host: e.smtp_host.clone(),
                smtp_port: e.smtp_port,
                username: e.username.clone(),
                password_env: e.password_env.clone(),
                password: e.password.clone(),
                from_address: e.from_address.clone(),
                feishu_app_id: e.feishu_app_id.clone(),
                feishu_app_secret_env: e.feishu_app_secret_env.clone(),
                feishu_app_secret: e.feishu_app_secret.clone(),
                feishu_from_address: e.feishu_from_address.clone(),
                feishu_region: e.feishu_region.clone(),
            }),
        auth_token: None,
        adaptive_routing: profile.config.adaptive_routing.clone(),
        voice: None,
        mode: Default::default(),
        tunnel_domain: None,
        base_domain: None,
        frps_server: None,
        allow_admin_shell: false,
        #[cfg(feature = "api")]
        dashboard_auth: None,
        #[cfg(feature = "api")]
        monitor: None,
        // F-005: credential pool + content routing are per-profile
        // fields on `ProfileConfig`; the flattened `Config` used by
        // gateway consumers does not currently surface them, so leave
        // these as `None`. Gateway runtime can still read them off
        // `profile.config` directly when needed.
        credential_pool: None,
        content_routing: profile.config.content_routing.clone(),
        // #1774: thread the profile's formatting opt-in so `octos serve`
        // sessions honor it (review: hardcoding false here left serve
        // permanently OFF while chat/gateway/acp worked).
        format_after_edit: profile.config.format_after_edit,
        appui: Default::default(),
        // Carry the profile-declared plugin loader policy through to the
        // flattened `Config` so callers reading
        // `config.plugins.require_signed` see the same value the profile
        // JSON declared. Defaults to permissive when the profile omits
        // the field.
        plugins: profile.config.plugins.clone(),
        // Startup CLI-flag defaults are not sourced from profile JSON — a
        // flattened profile Config always starts with an empty `cli` block.
        cli: Default::default(),
    }
}

/// Convert a `ChannelCredentials` to a octos `ChannelEntry` JSON value.
fn channel_to_entry(cred: &ChannelCredentials) -> serde_json::Value {
    match cred {
        ChannelCredentials::Telegram {
            token_env,
            allowed_senders,
        } => {
            let senders: Vec<&str> = allowed_senders
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();
            serde_json::json!({
                "type": "telegram",
                "allowed_senders": senders,
                "settings": { "token_env": token_env }
            })
        }
        ChannelCredentials::Discord { token_env } => serde_json::json!({
            "type": "discord",
            "settings": { "token_env": token_env }
        }),
        ChannelCredentials::DingTalk {
            webhook_url_env,
            secret_env,
            allowed_senders,
            webhook_port,
        } => {
            let senders: Vec<&str> = allowed_senders
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();
            let mut settings = serde_json::json!({
                "webhook_url_env": webhook_url_env,
                "secret_env": secret_env,
            });
            if let Some(port) = webhook_port {
                settings["webhook_port"] = serde_json::json!(port);
            }
            serde_json::json!({
                "type": "dingtalk",
                "allowed_senders": senders,
                "settings": settings,
            })
        }
        ChannelCredentials::Slack {
            bot_token_env,
            app_token_env,
        } => serde_json::json!({
            "type": "slack",
            "settings": { "bot_token_env": bot_token_env, "app_token_env": app_token_env }
        }),
        ChannelCredentials::WhatsApp { bridge_url } => serde_json::json!({
            "type": "whatsapp",
            "settings": { "bridge_url": bridge_url }
        }),
        ChannelCredentials::Feishu {
            app_id_env,
            app_secret_env,
            mode,
            region,
            webhook_port,
            verification_token_env,
            encrypt_key_env,
        } => {
            let mut settings = serde_json::json!({
                "app_id_env": app_id_env,
                "app_secret_env": app_secret_env,
            });
            if !mode.is_empty() {
                settings["mode"] = serde_json::json!(mode);
            }
            if !region.is_empty() {
                settings["region"] = serde_json::json!(region);
            }
            if let Some(port) = webhook_port {
                settings["webhook_port"] = serde_json::json!(port);
            }
            if !verification_token_env.is_empty() {
                settings["verification_token_env"] = serde_json::json!(verification_token_env);
            }
            if !encrypt_key_env.is_empty() {
                settings["encrypt_key_env"] = serde_json::json!(encrypt_key_env);
            }
            serde_json::json!({
                "type": "feishu",
                "settings": settings
            })
        }
        ChannelCredentials::Email {
            imap_host,
            imap_port,
            smtp_host,
            smtp_port,
            username_env,
            password_env,
        } => serde_json::json!({
            "type": "email",
            "settings": {
                "imap_host": imap_host,
                "imap_port": imap_port,
                "smtp_host": smtp_host,
                "smtp_port": smtp_port,
                "username_env": username_env,
                "password_env": password_env,
            }
        }),
        ChannelCredentials::Twilio {
            account_sid_env,
            auth_token_env,
            from_number,
            webhook_port,
        } => serde_json::json!({
            "type": "twilio",
            "settings": {
                "account_sid_env": account_sid_env,
                "auth_token_env": auth_token_env,
                "from_number": from_number,
                "webhook_port": webhook_port,
            }
        }),
        ChannelCredentials::Api { port, auth_token } => {
            let mut settings = serde_json::json!({"port": port});
            if let Some(token) = auth_token {
                settings["auth_token"] = serde_json::json!(token);
            }
            serde_json::json!({
                "type": "api",
                "settings": settings
            })
        }
        ChannelCredentials::WeComBot { bot_id, secret_env } => serde_json::json!({
            "type": "wecom-bot",
            "settings": {
                "bot_id": bot_id,
                "secret_env": secret_env,
            }
        }),
        ChannelCredentials::Matrix {
            homeserver,
            as_token,
            hs_token,
            server_name,
            sender_localpart,
            user_prefix,
            port,
            allowed_senders,
            mention_only,
            mode,
            user_id,
            access_token,
            password,
            device_name,
            rooms,
            auto_join,
            auto_join_allowlist,
            group_policy,
            require_mention,
        } => {
            let mut settings = serde_json::json!({ "homeserver": homeserver });
            if mode.eq_ignore_ascii_case("user") {
                // User-account (client) mode: log in as a regular Matrix user
                // and long-poll `/sync`. Only emit the credentials that are set.
                settings["mode"] = serde_json::json!("user");
                settings["auto_join"] = serde_json::json!(auto_join);
                settings["group_policy"] = serde_json::json!(group_policy);
                settings["require_mention"] = serde_json::json!(require_mention);
                if !user_id.is_empty() {
                    settings["user_id"] = serde_json::json!(user_id);
                }
                if !access_token.is_empty() {
                    settings["access_token"] = serde_json::json!(access_token);
                }
                if !password.is_empty() {
                    settings["password"] = serde_json::json!(password);
                }
                if !device_name.is_empty() {
                    settings["device_name"] = serde_json::json!(device_name);
                }
                if !rooms.is_empty() {
                    settings["rooms"] = serde_json::json!(rooms);
                }
                if !auto_join_allowlist.is_empty() {
                    settings["auto_join_allowlist"] = serde_json::json!(auto_join_allowlist);
                }
            } else {
                // Appservice mode (default): homeserver-side registration.
                settings["as_token"] = serde_json::json!(as_token);
                settings["hs_token"] = serde_json::json!(hs_token);
                settings["server_name"] = serde_json::json!(server_name);
                settings["sender_localpart"] = serde_json::json!(sender_localpart);
                settings["user_prefix"] = serde_json::json!(user_prefix);
                settings["port"] = serde_json::json!(port);
                settings["mention_only"] = serde_json::json!(mention_only);
            }
            serde_json::json!({
                "type": "matrix",
                "allowed_senders": allowed_senders,
                "settings": settings,
            })
        }
        ChannelCredentials::QQBot {
            app_id,
            client_secret_env,
        } => serde_json::json!({
            "type": "qq-bot",
            "settings": {
                "app_id": app_id,
                "client_secret_env": client_secret_env,
            }
        }),
        ChannelCredentials::WeChat {
            token_env,
            base_url,
        } => serde_json::json!({
            "type": "wechat",
            "settings": {
                "token_env": token_env,
                "base_url": base_url,
            }
        }),
        ChannelCredentials::Line {
            channel_secret_env,
            channel_access_token_env,
            allowed_senders,
            webhook_port,
            require_mention,
            bot_user_id,
        } => {
            let senders: Vec<&str> = allowed_senders
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();
            let mut settings = serde_json::json!({
                "channel_secret_env": channel_secret_env,
                "channel_access_token_env": channel_access_token_env,
                "require_mention": require_mention,
            });
            if let Some(port) = webhook_port {
                settings["webhook_port"] = serde_json::json!(port);
            }
            if !bot_user_id.is_empty() {
                settings["bot_user_id"] = serde_json::json!(bot_user_id);
            }
            serde_json::json!({
                "type": "line",
                "allowed_senders": senders,
                "settings": settings,
            })
        }
    }
}

/// Classification of changes between two profile versions.
#[derive(Debug)]
pub enum ProfileChange {
    /// No meaningful change detected.
    Unchanged,
    /// Only hot-reloadable fields changed (gateway's own watcher handles these).
    HotReloadable,
    /// Fields changed that require a gateway restart.
    RestartRequired(Vec<String>),
}

/// Compare two profiles and classify the nature of changes.
///
/// Restart-required: llm, review, search, deep_crawl, apps, robot, channels,
///   env_vars, email, hooks, sandbox, routing, credential_pool, plugins.
/// Hot-reloadable: system_prompt, max_history, max_iterations,
///   max_concurrent_sessions, browser_timeout_secs.
pub fn diff_profiles(old: &UserProfile, new: &UserProfile) -> ProfileChange {
    let mut restart_fields = Vec::new();
    let oc = &old.config;
    let nc = &new.config;

    // Restart-required: parent_id change
    if old.parent_id != new.parent_id {
        restart_fields.push("parent_id".into());
    }

    if oc.llm != nc.llm {
        restart_fields.push("llm".into());
    }
    if oc.review != nc.review {
        restart_fields.push("review".into());
    }
    if oc.search != nc.search {
        restart_fields.push("search".into());
    }
    if oc.deep_crawl != nc.deep_crawl {
        restart_fields.push("deep_crawl".into());
    }
    if oc.apps != nc.apps {
        restart_fields.push("apps".into());
    }
    if oc.robot != nc.robot {
        restart_fields.push("robot".into());
    }
    if oc.channels != nc.channels {
        restart_fields.push("channels".into());
    }
    if oc.env_vars != nc.env_vars {
        restart_fields.push("env_vars".into());
    }
    if oc.email != nc.email {
        restart_fields.push("email".into());
    }
    if oc.hooks != nc.hooks {
        restart_fields.push("hooks".into());
    }
    if oc.admin_mode != nc.admin_mode {
        restart_fields.push("admin_mode".into());
    }
    if oc.sandbox != nc.sandbox {
        restart_fields.push("sandbox".into());
    }
    if oc.adaptive_routing != nc.adaptive_routing {
        restart_fields.push("adaptive_routing".into());
    }
    if oc.cost_budget != nc.cost_budget {
        restart_fields.push("cost_budget".into());
    }
    if oc.matrix != nc.matrix {
        restart_fields.push("matrix".into());
    }
    if oc.content_routing != nc.content_routing {
        restart_fields.push("content_routing".into());
    }
    if oc.credential_pool != nc.credential_pool {
        restart_fields.push("credential_pool".into());
    }
    // Section B (codex review round-6): plugin loader policy changes
    // (e.g. flipping `plugins.require_signed`) only take effect during
    // bootstrap, so a toggle must trigger a gateway restart to flush
    // the stale plugin registry and apply the new gate.
    if oc.plugins != nc.plugins {
        restart_fields.push("plugins".into());
    }
    if oc.lane_routing != nc.lane_routing {
        restart_fields.push("lane_routing".into());
    }

    if !restart_fields.is_empty() {
        return ProfileChange::RestartRequired(restart_fields);
    }

    // Hot-reloadable fields
    if oc.gateway != nc.gateway {
        return ProfileChange::HotReloadable;
    }

    ProfileChange::Unchanged
}

/// Check if a profile has a Feishu channel and return its webhook port configuration.
///
/// Returns:
/// - `Some(Some(port))` — Feishu channel exists with explicit webhook port
/// - `Some(None)` — Feishu channel exists but needs an auto-assigned port
/// - `None` — no Feishu channel
pub fn feishu_webhook_port(profile: &UserProfile) -> Option<Option<u16>> {
    for ch in &profile.config.channels {
        if let ChannelCredentials::Feishu {
            mode, webhook_port, ..
        } = ch
        {
            if mode == "webhook" {
                return Some(*webhook_port);
            }
        }
    }
    None
}

/// Check if a profile has a LINE channel and return its webhook port configuration.
///
/// Returns:
/// - `Some(Some(port))` — LINE channel exists with explicit webhook port
/// - `Some(None)` — LINE channel exists but needs an auto-assigned port
/// - `None` — no LINE channel
pub fn line_webhook_port(profile: &UserProfile) -> Option<Option<u16>> {
    for ch in &profile.config.channels {
        if let ChannelCredentials::Line { webhook_port, .. } = ch {
            return Some(*webhook_port);
        }
    }
    None
}

/// Check if a profile has a DingTalk channel and return its webhook port configuration.
///
/// Returns:
/// - `Some(Some(port))` — DingTalk channel exists with explicit webhook port
/// - `Some(None)` — DingTalk channel exists but needs an auto-assigned port
/// - `None` — no DingTalk channel
pub fn dingtalk_webhook_port(profile: &UserProfile) -> Option<Option<u16>> {
    for ch in &profile.config.channels {
        if let ChannelCredentials::DingTalk { webhook_port, .. } = ch {
            return Some(*webhook_port);
        }
    }
    None
}

/// Webhook port needed by any profile channel that listens for HTTP webhooks.
pub fn profile_webhook_port(profile: &UserProfile) -> Option<Option<u16>> {
    feishu_webhook_port(profile)
        .or_else(|| line_webhook_port(profile))
        .or_else(|| dingtalk_webhook_port(profile))
}

/// Get the API channel port from a profile, if one is configured.
pub fn api_channel_port(profile: &UserProfile) -> Option<u16> {
    for ch in &profile.config.channels {
        if let ChannelCredentials::Api { port, .. } = ch {
            return Some(*port);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_include_bridge_url_in_env_vars_when_set() {
        let config = SmartHomeConfig {
            bridge_url: Some("http://localhost:8787".into()),
            ..Default::default()
        };
        let env_vars = HashMap::new();
        let out = config.to_env_vars(&env_vars);
        assert!(
            out.contains(&(
                "SMART_HOME_BRIDGE_URL".to_string(),
                "http://localhost:8787".to_string()
            )),
            "expected SMART_HOME_BRIDGE_URL in {out:?}"
        );
    }

    #[test]
    fn should_resolve_token_env_when_literal_token_absent() {
        let config = SmartHomeConfig {
            bridge_url: None,
            token: None,
            token_env: Some("SH_TOKEN".into()),
        };
        let mut env_vars = HashMap::new();
        env_vars.insert("SH_TOKEN".to_string(), "secret123".to_string());
        let out = config.to_env_vars(&env_vars);
        assert!(
            out.contains(&(
                "SMART_HOME_BRIDGE_TOKEN".to_string(),
                "secret123".to_string()
            )),
            "expected resolved token in {out:?}"
        );
    }

    #[test]
    fn should_prefer_literal_token_over_token_env() {
        let config = SmartHomeConfig {
            bridge_url: None,
            token: Some("literal-token".into()),
            token_env: Some("SH_TOKEN".into()),
        };
        let mut env_vars = HashMap::new();
        env_vars.insert("SH_TOKEN".to_string(), "should-not-be-used".to_string());
        let out = config.to_env_vars(&env_vars);
        assert!(
            out.contains(&(
                "SMART_HOME_BRIDGE_TOKEN".to_string(),
                "literal-token".to_string()
            )),
            "expected literal token to win in {out:?}"
        );
    }

    #[test]
    fn should_mask_smart_home_token_but_not_url_when_masking_profile() {
        let profile = UserProfile {
            id: "test".into(),
            name: "Test".into(),
            public_subdomain: None,
            enabled: true,
            data_dir: None,
            parent_id: None,
            config: ProfileConfig {
                smart_home: Some(SmartHomeConfig {
                    bridge_url: Some("http://192.168.1.50:8787".into()),
                    token: Some("supersecret-token-value".into()),
                    token_env: None,
                }),
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let profile = mask_secrets(&profile);
        let smart_home = profile.config.smart_home.expect("smart_home present");
        assert_eq!(
            smart_home.bridge_url.as_deref(),
            Some("http://192.168.1.50:8787"),
            "bridge_url is not a secret and must not be masked"
        );
        let masked_token = smart_home.token.expect("token present");
        assert_ne!(masked_token, "supersecret-token-value");
        assert_eq!(masked_token, "supe***lue");
    }

    #[test]
    fn should_apply_smart_home_patch_value_and_clear() {
        let mut config = ProfileConfig::default();
        let patch = ProfileConfigPatch {
            smart_home: PatchField::Value(SmartHomeConfig {
                bridge_url: Some("http://localhost:8787".into()),
                token: None,
                token_env: None,
            }),
            ..Default::default()
        };
        config.apply_patch(patch);
        assert_eq!(
            config
                .smart_home
                .as_ref()
                .and_then(|s| s.bridge_url.clone()),
            Some("http://localhost:8787".to_string())
        );

        let clear_patch = ProfileConfigPatch {
            smart_home: PatchField::Clear,
            ..Default::default()
        };
        config.apply_patch(clear_patch);
        assert!(config.smart_home.is_none());
    }

    fn llm_selection(
        family_id: &str,
        model_id: &str,
        api_key_env: Option<&str>,
        base_url: Option<&str>,
    ) -> LlmModelSelectionConfig {
        LlmModelSelectionConfig {
            family_id: Some(family_id.into()),
            model_id: Some(model_id.into()),
            route: Some(LlmRouteConfig {
                route_id: None,
                label: None,
                base_url: base_url.map(str::to_string),
                api_key_env: api_key_env.map(str::to_string),
                api_type: None,
            }),
            ..Default::default()
        }
    }

    fn llm_profile(
        primary: LlmModelSelectionConfig,
        fallbacks: Vec<LlmModelSelectionConfig>,
    ) -> LlmProfileConfig {
        LlmProfileConfig {
            primary: Some(primary),
            fallbacks,
        }
    }

    #[test]
    fn test_validate_profile_id() {
        assert!(validate_profile_id("alice").is_ok());
        assert!(validate_profile_id("team-bot").is_ok());
        assert!(validate_profile_id("user123").is_ok());
        assert!(validate_profile_id("").is_err());
        assert!(validate_profile_id("-bad").is_err());
        // Channel names are reserved (codex #1613 r4): a profile named
        // after a channel makes profiled session keys ambiguous.
        assert!(validate_profile_id("api").is_err());
        assert!(validate_profile_id("slack").is_err());
        assert!(validate_profile_id("line").is_err());
        assert!(validate_profile_id("bad-").is_err());
        assert!(validate_profile_id("UPPER").is_err());
        assert!(validate_profile_id("has space").is_err());
        assert!(validate_profile_id("a".repeat(65).as_str()).is_err());
    }

    #[test]
    fn should_allow_channel_named_public_subdomains() {
        // codex #1613 r5 P2: the channel-name reservation exists to keep
        // the SessionKey PROFILE segment unambiguous. A public subdomain
        // never occupies that segment (it resolves to the owning
        // profile's real id), so channel names stay valid slugs here.
        assert!(validate_public_subdomain("slack").is_ok());
        assert!(validate_public_subdomain("telegram").is_ok());
        assert!(validate_public_subdomain("line").is_ok());
        // The subdomain namespace keeps its OWN reserved list.
        assert!(validate_public_subdomain("api").is_err());
        assert!(validate_public_subdomain("www").is_err());
        assert!(validate_public_subdomain("admin").is_err());
        // Shape checks still shared with profile ids.
        assert!(validate_public_subdomain("-bad").is_err());
        assert!(validate_public_subdomain("UPPER").is_err());
    }

    #[test]
    fn registration_reservation_tracks_loadability() {
        // codex #1613 r6/r8: the probe behind AuthManager's generated-
        // id loop. Anonymous registration never claims an existing
        // file; an authorized (allowlist) claim passes only a
        // cleanly-loadable record — a corrupt file stays reserved, or
        // the verify path's auto-create (get error → None → save)
        // would overwrite it with a default profile (r8 P2).
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::open_unified(dir.path()).unwrap();

        // Absent: free for anyone.
        assert!(!store.id_reserved_for_registration("ghost", false));
        assert!(!store.id_reserved_for_registration("ghost", true));

        // Loadable pre-provisioned profile: reserved from anonymous,
        // claimable with authorization.
        let invitee = UserProfile {
            id: "invitee".into(),
            name: "Invitee".into(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig::default(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        store.save(&invitee).unwrap();
        assert!(store.id_reserved_for_registration("invitee", false));
        assert!(!store.id_reserved_for_registration("invitee", true));

        // Corrupt record: reserved from EVERYONE.
        std::fs::write(store.profile_path("mangled"), "{not json").unwrap();
        assert!(store.id_reserved_for_registration("mangled", false));
        assert!(store.id_reserved_for_registration("mangled", true));
    }

    #[test]
    fn default_profile_pointer_round_trips_and_defaults_to_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::open_unified(dir.path()).unwrap();

        // Unset: no pointer file yet.
        assert_eq!(store.default_profile(), None);

        store.set_default_profile("glm").unwrap();
        assert_eq!(store.default_profile().as_deref(), Some("glm"));

        // Overwrite replaces the prior pointer.
        store.set_default_profile("deepseek").unwrap();
        assert_eq!(store.default_profile().as_deref(), Some("deepseek"));

        // Whitespace is trimmed on both write and read; an all-whitespace
        // pointer reads back as unset rather than a blank id.
        store.set_default_profile("  kimi \n").unwrap();
        assert_eq!(store.default_profile().as_deref(), Some("kimi"));
        std::fs::write(store.default_profile_pointer_path(), "   \n").unwrap();
        assert_eq!(store.default_profile(), None);
    }

    #[test]
    fn should_quarantine_legacy_channel_named_profiles_on_load() {
        // codex #1613 r5 P1: profiles created BEFORE the channel-name
        // reservation bypass save-time validation forever — an existing
        // `profiles/api.json` would keep producing `api:<channel>:<chat>`
        // keys that `split_base_key` mis-parses (profile dropped,
        // mis-scoped forks). Loading must fail fast instead: `get`
        // errors with rename guidance, `list` skips the record (same
        // precedent as unparsable profile JSON) so one legacy record
        // cannot brick a multi-profile deployment.
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::open_unified(dir.path()).unwrap();

        // Plant the legacy record directly on disk, bypassing save().
        let legacy = serde_json::json!({
            "id": "api",
            "name": "Legacy Api",
            "enabled": true,
            "created_at": "2025-01-01T00:00:00Z",
            "updated_at": "2025-01-01T00:00:00Z",
            "config": {}
        });
        std::fs::write(
            store.profile_path("api"),
            serde_json::to_string_pretty(&legacy).unwrap(),
        )
        .unwrap();

        // A healthy profile sits alongside it.
        let healthy = UserProfile {
            id: "alice".into(),
            name: "Alice".into(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig::default(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        store.save(&healthy).unwrap();

        let err = store
            .get("api")
            .expect_err("channel-named legacy profile must fail fast on get");
        assert!(
            err.to_string().contains("reserved channel name"),
            "error must explain the rename requirement, got: {err}"
        );

        let listed = store.list().unwrap();
        assert!(
            listed.iter().any(|p| p.id == "alice"),
            "healthy profiles must survive a legacy record"
        );
        assert!(
            !listed.iter().any(|p| p.id == "api"),
            "channel-named legacy profile must be skipped from list()"
        );
    }

    #[test]
    fn test_profile_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::open_unified(dir.path()).unwrap();

        let profile = UserProfile {
            // Not "test" — a reserved channel name (codex #1613 r4).
            id: "test-bot".into(),
            name: "Test Bot".into(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                llm: Some(llm_profile(
                    llm_selection(
                        "anthropic",
                        "claude-sonnet-4-20250514",
                        Some("ANTHROPIC_API_KEY"),
                        None,
                    ),
                    vec![],
                )),
                channels: vec![ChannelCredentials::Telegram {
                    token_env: "TG_TOKEN".into(),
                    allowed_senders: String::new(),
                }],
                gateway: GatewaySettings {
                    max_history: Some(50),
                    ..Default::default()
                },
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        store.save(&profile).unwrap();
        let loaded = store.get("test-bot").unwrap().unwrap();
        assert_eq!(loaded.id, "test-bot");
        assert_eq!(loaded.name, "Test Bot");
        assert!(loaded.enabled);

        let profiles = store.list().unwrap();
        assert_eq!(profiles.len(), 1);

        assert!(store.delete("test-bot").unwrap());
        assert!(store.get("test-bot").unwrap().is_none());
    }

    #[test]
    fn test_save_preserves_local_owner_metadata_fields() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::open_unified(dir.path()).unwrap();
        let mut profile = UserProfile {
            id: "ada".into(),
            name: "Ada Lovelace".into(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig::default(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        store.save(&profile).unwrap();
        let path = store.profile_path("ada");
        let mut raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        raw.as_object_mut()
            .unwrap()
            .insert("username".into(), serde_json::json!("ada"));
        raw.as_object_mut()
            .unwrap()
            .insert("email".into(), serde_json::json!("ada@example.com"));
        std::fs::write(&path, serde_json::to_string_pretty(&raw).unwrap()).unwrap();

        profile.name = "Ada Byron".into();
        store.save(&profile).unwrap();

        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["username"], serde_json::json!("ada"));
        assert_eq!(saved["email"], serde_json::json!("ada@example.com"));
        assert_eq!(saved["name"], serde_json::json!("Ada Byron"));
    }

    #[test]
    fn config_from_profile_maps_sub_providers_for_isolated_pipeline_lanes() {
        // The `deep_research` pipeline's `cheap`/`strong` nodes resolve through
        // the profile's `sub_providers`; `config_from_profile` must carry them
        // into the runtime `Config`. It used to hard-zero them (`vec![]`), so
        // serve-mode pipelines could never reach an isolated research lane.
        let sp = |key: &str, provider: &str, model: &str| crate::config::SubProviderConfig {
            key: key.into(),
            provider: provider.into(),
            model: Some(model.into()),
            api_key_env: None,
            base_url: None,
            description: None,
            default_context_window: None,
            max_output_tokens: None,
            api_type: None,
        };
        let profile = UserProfile {
            id: "research-lane".into(),
            name: "Research Lane".into(),
            enabled: false,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                sub_providers: vec![
                    sp("cheap", "gemini", "gemini-2.5-flash"),
                    sp("strong", "openai", "gpt-5-mini"),
                ],
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let config = config_from_profile(&profile, None, None);
        assert_eq!(
            config.sub_providers.len(),
            2,
            "profile sub_providers must reach the runtime config (was hard-zeroed)"
        );
        assert_eq!(config.sub_providers[0].key, "cheap");
        assert_eq!(
            config.sub_providers[0].model.as_deref(),
            Some("gemini-2.5-flash")
        );
        assert_eq!(config.sub_providers[1].key, "strong");
    }

    /// #24 — REAL-machine verification that the LIVE profile config
    /// (`~/.octos/profiles/octos.json`, the registry the running serve actually
    /// loads) carries the `zai` + `goal_verifier` lanes written by the #24
    /// fix, and that both resolve to a buildable provider through the REAL
    /// config-from-profile path — not a test-constructed one (S2/S3's gap).
    /// Gated `#[ignore]` so CI never depends on the operator's live box; run:
    ///   cargo test -p octos-cli --lib --features api -- --ignored --exact \
    ///     profiles::tests::live_profile_carries_zai_and_goal_verifier_lanes
    #[test]
    #[ignore = "reads the operator's live ~/.octos/profiles/octos.json; run explicitly"]
    fn live_profile_carries_zai_and_goal_verifier_lanes() {
        let path = dirs::home_dir()
            .expect("home dir")
            .join(".octos")
            .join("profiles")
            .join("octos.json");
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("live profile unreadable at {}: {e}", path.display()));
        let profile: UserProfile =
            serde_json::from_str(&content).expect("live profile must deserialize into UserProfile");

        // Both lanes present in the LIVE config.
        let keys: Vec<&str> = profile
            .config
            .sub_providers
            .iter()
            .map(|sp| sp.key.as_str())
            .collect();
        assert!(
            keys.contains(&"zai"),
            "zai lane missing from live config: {keys:?}"
        );
        assert!(
            keys.contains(&"goal_verifier"),
            "goal_verifier lane missing from live config: {keys:?}"
        );

        // The zai lane carries the right model through the REAL
        // config-from-profile mapping.
        let config = config_from_profile(&profile, None, None);
        let zai = config
            .sub_providers
            .iter()
            .find(|sp| sp.key == "zai")
            .expect("zai lane survives config_from_profile");
        assert_eq!(zai.provider, "zai");
        assert_eq!(zai.model.as_deref(), Some("glm-5.2"));

        // The goal_verifier lane BUILDS a provider via the #1935 path — the
        // exact call that was returning None (empty verifier) before the lane
        // was configured. This needs the lane's credential in env to fully
        // build; without it we assert the lane is at least FOUND (not None
        // because the key is absent).
        let verifier_lane = config
            .sub_providers
            .iter()
            .find(|sp| sp.key == crate::runtime::profile::GOAL_VERIFIER_LANE_KEY);
        assert!(
            verifier_lane.is_some(),
            "goal_verifier lane must be FOUND (its absence was the empty-verifier root cause)"
        );
    }

    #[test]
    fn config_from_profile_threads_format_after_edit() {
        // #1774 review: `octos serve` builds session configs through
        // config_from_profile — hardcoding `format_after_edit: false` here
        // left serve permanently OFF while chat/gateway/acp honored the
        // opt-in. The profile's flag must reach the runtime Config.
        let profile = UserProfile {
            id: "fmt-opt-in".into(),
            name: "Fmt Opt-In".into(),
            enabled: false,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                format_after_edit: true,
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        assert!(
            config_from_profile(&profile, None, None).format_after_edit,
            "profile opt-in must reach the runtime config (serve path)"
        );
        // And the default stays OFF.
        let off = UserProfile {
            config: ProfileConfig::default(),
            ..profile
        };
        assert!(!config_from_profile(&off, None, None).format_after_edit);
    }

    #[test]
    fn config_from_profile_threads_tool_policy() {
        // #2168: config_from_profile hardcoded `tool_policy: None`, so a serve /
        // UserProfile session could never slim its tool roster (the lean #2133
        // roster only reaches the built-in `coding` ProfileDefinition). A
        // profile-level tool_policy must now reach the runtime Config, where the
        // serve path already applies it.
        let policy: octos_agent::ToolPolicy = serde_json::from_value(serde_json::json!({
            "allow": ["read_file", "write_file", "group:runtime", "check", "update_plan"]
        }))
        .expect("valid tool policy");
        let profile = UserProfile {
            id: "lean-serve".into(),
            name: "Lean Serve".into(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                tool_policy: Some(policy.clone()),
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        assert_eq!(
            config_from_profile(&profile, None, None).tool_policy,
            Some(policy),
            "a profile tool_policy must reach the runtime config (serve applies it)"
        );
        // A profile without one stays None (no filtering) — unchanged behavior.
        let off = UserProfile {
            config: ProfileConfig::default(),
            ..profile
        };
        assert_eq!(config_from_profile(&off, None, None).tool_policy, None);
    }

    #[test]
    fn tool_policy_inherits_from_defaults_but_own_wins() {
        // #2168 review (item 4): tool_policy inherits like its sibling Option
        // fields — an operator profile-default applies when the profile has
        // none, and the profile's own always wins.
        let default_policy: octos_agent::ToolPolicy =
            serde_json::from_value(serde_json::json!({ "deny": ["group:web"] })).unwrap();
        let own_policy: octos_agent::ToolPolicy =
            serde_json::from_value(serde_json::json!({ "allow": ["read_file"] })).unwrap();
        let defaults = ProfileConfig {
            tool_policy: Some(default_policy.clone()),
            ..Default::default()
        };
        // No own policy -> inherits the default.
        let inherited = merge_profile_defaults(&ProfileConfig::default(), &defaults);
        assert_eq!(inherited.tool_policy, Some(default_policy));
        // Own policy set -> the profile wins.
        let with_own = ProfileConfig {
            tool_policy: Some(own_policy.clone()),
            ..Default::default()
        };
        let merged = merge_profile_defaults(&with_own, &defaults);
        assert_eq!(merged.tool_policy, Some(own_policy));
    }

    #[test]
    fn test_config_from_profile() {
        let profile = UserProfile {
            id: "gen-test".into(),
            name: "Config Gen".into(),
            enabled: false,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                llm: Some(llm_profile(
                    llm_selection("openai", "gpt-4o", None, None),
                    vec![],
                )),
                channels: vec![
                    ChannelCredentials::Telegram {
                        token_env: "TG".into(),
                        allowed_senders: String::new(),
                    },
                    ChannelCredentials::Slack {
                        bot_token_env: "SB".into(),
                        app_token_env: "SA".into(),
                    },
                ],
                gateway: GatewaySettings {
                    max_history: Some(100),
                    system_prompt: Some("Hello".into()),
                    ..Default::default()
                },
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let config = config_from_profile(&profile, None, None);
        assert_eq!(config.provider.as_deref(), Some("openai"));
        assert_eq!(config.model.as_deref(), Some("gpt-4o"));
        let gw = config.gateway.unwrap();
        assert_eq!(gw.max_history, 100);
        assert_eq!(gw.system_prompt.as_deref(), Some("Hello"));
        assert_eq!(gw.channels.len(), 2);
    }

    #[test]
    fn test_config_from_profile_provider_passthrough() {
        let profile = UserProfile {
            id: "moonshot-test".into(),
            name: "Moonshot".into(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                llm: Some(llm_profile(
                    llm_selection("moonshot", "kimi-k2.5", Some("MOONSHOT_API_KEY"), None),
                    vec![],
                )),
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let config = config_from_profile(&profile, None, None);
        assert_eq!(config.provider.as_deref(), Some("moonshot"));
        assert!(config.base_url.is_none());
        assert_eq!(config.model.as_deref(), Some("kimi-k2.5"));
    }

    #[test]
    fn test_save_persists_structured_llm_contract() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::open_unified(dir.path()).unwrap();

        let profile = UserProfile {
            id: "legacy-llm".into(),
            name: "Legacy LLM".into(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                llm: Some(llm_profile(
                    llm_selection(
                        "moonshot",
                        "kimi-k2.5",
                        Some("AUTODL_API_KEY"),
                        Some("https://www.autodl.art/api/v1"),
                    ),
                    vec![LlmModelSelectionConfig {
                        family_id: Some("minimax".into()),
                        model_id: Some("MiniMax-M2.5-highspeed".into()),
                        route: Some(LlmRouteConfig {
                            route_id: Some("wisemodel".into()),
                            label: Some("WiseModel".into()),
                            base_url: Some("https://api.wisemodel.cn/v1".into()),
                            api_key_env: Some("WISEMODEL_API_KEY".into()),
                            api_type: Some("openai".into()),
                        }),
                        cost_per_m: Some(3.2),
                        strong: Some(true),
                        ..Default::default()
                    }],
                )),
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        store.save(&profile).unwrap();
        let loaded = store.get("legacy-llm").unwrap().unwrap();
        let llm = loaded.config.llm.expect("normalized llm contract");
        let primary = llm.primary.expect("primary selection");
        assert_eq!(primary.family_id.as_deref(), Some("moonshot"));
        assert_eq!(primary.model_id.as_deref(), Some("kimi-k2.5"));
        assert_eq!(
            primary.route.and_then(|route| route.base_url).as_deref(),
            Some("https://www.autodl.art/api/v1")
        );
        assert_eq!(llm.fallbacks.len(), 1);
        assert_eq!(llm.fallbacks[0].family_id.as_deref(), Some("minimax"));
        assert_eq!(
            llm.fallbacks[0].model_id.as_deref(),
            Some("MiniMax-M2.5-highspeed")
        );
    }

    #[test]
    fn test_config_from_profile_uses_structured_llm_contract() {
        let profile = UserProfile {
            id: "structured-llm".into(),
            name: "Structured LLM".into(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                llm: Some(LlmProfileConfig {
                    primary: Some(LlmModelSelectionConfig {
                        family_id: Some("moonshot".into()),
                        model_id: Some("kimi-k2.5".into()),
                        route: Some(LlmRouteConfig {
                            route_id: Some("autodl".into()),
                            label: Some("AutoDL".into()),
                            base_url: Some("https://www.autodl.art/api/v1".into()),
                            api_key_env: Some("AUTODL_API_KEY".into()),
                            api_type: Some("openai".into()),
                        }),
                        model_hints: Some(octos_llm::openai::ModelHints {
                            uses_completion_tokens: true,
                            fixed_temperature: false,
                            lacks_vision: false,
                            merge_system_messages: false,
                            reasoning_style: octos_llm::openai::ReasoningStyle::None,
                        }),
                        cost_per_m: Some(4.5),
                        strong: Some(true),
                        // #2142: operator window override on the primary.
                        context_window: Some(16_384),
                    }),
                    fallbacks: vec![LlmModelSelectionConfig {
                        family_id: Some("minimax".into()),
                        model_id: Some("MiniMax-M2.5-highspeed".into()),
                        route: Some(LlmRouteConfig {
                            route_id: Some("wisemodel".into()),
                            label: Some("WiseModel".into()),
                            base_url: Some("https://api.wisemodel.cn/v1".into()),
                            api_key_env: Some("WISEMODEL_API_KEY".into()),
                            api_type: Some("openai".into()),
                        }),
                        model_hints: Some(octos_llm::openai::ModelHints {
                            uses_completion_tokens: false,
                            fixed_temperature: false,
                            lacks_vision: false,
                            merge_system_messages: true,
                            reasoning_style: octos_llm::openai::ReasoningStyle::None,
                        }),
                        cost_per_m: Some(3.2),
                        strong: Some(true),
                        // #2142: a DIFFERENT per-fallback window override —
                        // must project independently of the primary's.
                        context_window: Some(8_192),
                    }],
                }),
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let config = config_from_profile(&profile, None, None);
        assert_eq!(config.provider.as_deref(), Some("moonshot"));
        assert_eq!(config.model.as_deref(), Some("kimi-k2.5"));
        assert_eq!(
            config.base_url.as_deref(),
            Some("https://www.autodl.art/api/v1")
        );
        assert_eq!(config.api_key_env.as_deref(), Some("AUTODL_API_KEY"));
        assert_eq!(config.api_type.as_deref(), Some("openai"));
        assert_eq!(
            config
                .model_hints
                .as_ref()
                .map(|h| h.uses_completion_tokens),
            Some(true)
        );
        assert_eq!(config.fallback_models.len(), 1);
        assert_eq!(config.fallback_models[0].provider.as_str(), "minimax");
        assert_eq!(
            config.fallback_models[0].model.as_deref(),
            Some("MiniMax-M2.5-highspeed")
        );
        assert_eq!(
            config.fallback_models[0].base_url.as_deref(),
            Some("https://api.wisemodel.cn/v1")
        );
        assert_eq!(
            config.fallback_models[0]
                .model_hints
                .as_ref()
                .map(|h| h.merge_system_messages),
            Some(true)
        );
        // #2142: the per-selection context_window overrides project through
        // to the flattened Config — primary onto `config.context_window`, and
        // each fallback onto its own `FallbackModel.context_window`,
        // independently (16384 vs 8192).
        assert_eq!(config.context_window, Some(16_384));
        assert_eq!(config.fallback_models[0].context_window, Some(8_192));
    }

    #[test]
    fn test_profile_config_patch_applies_typed_sections_without_wiping_gateway() {
        let mut config = ProfileConfig {
            gateway: GatewaySettings {
                max_history: Some(42),
                system_prompt: Some("keep me".into()),
                ..Default::default()
            },
            ..Default::default()
        };

        config.apply_patch(ProfileConfigPatch {
            gateway: Some(GatewaySettingsPatch {
                max_history: PatchField::Value(100),
                ..Default::default()
            }),
            search: PatchField::Value(SearchConfig {
                providers: [(
                    "tavily".into(),
                    SearchProviderConfig {
                        api_key_env: Some("TAVILY_API_KEY".into()),
                    },
                )]
                .into(),
            }),
            deep_crawl: PatchField::Value(DeepCrawlConfig {
                page_settle_ms: Some(1500),
                max_output_chars: Some(32_000),
            }),
            apps: PatchField::Value(AppsConfig {
                slides: Some(SlidesAppConfig {
                    template_dir: Some("/opt/octos/slides".into()),
                    default_theme: Some("crew".into()),
                }),
            }),
            ..Default::default()
        });

        assert_eq!(config.gateway.max_history, Some(100));
        assert_eq!(config.gateway.system_prompt.as_deref(), Some("keep me"));
        assert_eq!(
            config
                .search
                .as_ref()
                .and_then(|search| search.providers.get("tavily"))
                .and_then(|provider| provider.api_key_env.as_deref()),
            Some("TAVILY_API_KEY")
        );
        assert_eq!(
            config
                .deep_crawl
                .as_ref()
                .and_then(|cfg| cfg.page_settle_ms),
            Some(1500)
        );
        assert_eq!(
            config
                .apps
                .as_ref()
                .and_then(|apps| apps.slides.as_ref())
                .and_then(|slides| slides.default_theme.as_deref()),
            Some("crew")
        );
    }

    #[test]
    fn test_profile_config_patch_persists_home_dashboard_json() {
        let mut config = ProfileConfig::default();
        let home = serde_json::json!({
            "settings": {
                "city": "Tokyo",
                "clock_format": "24h",
                "idle_seconds": 45
            },
            "events": [
                { "id": "dinner", "title": "Dinner", "date": "2026-06-16", "time": "19:30" }
            ],
            "metro_layout": {
                "clock": { "col": 1, "row": 1, "w": 4, "h": 2 }
            }
        });

        config.apply_patch(ProfileConfigPatch {
            home: PatchField::Value(home.clone()),
            ..Default::default()
        });

        assert_eq!(config.home.as_ref(), Some(&home));

        config.apply_patch(ProfileConfigPatch {
            home: PatchField::Clear,
            ..Default::default()
        });

        assert!(config.home.is_none());
    }

    #[test]
    fn test_profile_config_patch_updates_plugin_and_lane_policy() {
        let mut config = ProfileConfig::default();
        let mut lane_routing = octos_llm::LaneRoutingConfig::default();
        lane_routing
            .topic_lanes
            .insert("code".into(), octos_llm::Lane::CodeCapable);

        config.apply_patch(ProfileConfigPatch {
            plugins: Some(crate::config::PluginsConfig {
                require_signed: true,
            }),
            lane_routing: PatchField::Value(lane_routing.clone()),
            ..Default::default()
        });

        assert!(config.plugins.require_signed);
        assert_eq!(config.lane_routing.as_ref(), Some(&lane_routing));

        config.apply_patch(ProfileConfigPatch {
            lane_routing: PatchField::Clear,
            ..Default::default()
        });

        assert!(config.lane_routing.is_none());
    }

    #[test]
    fn test_profile_config_patch_replaces_review_contract() {
        let mut config = ProfileConfig::default();

        config.apply_patch(ProfileConfigPatch {
            review: PatchField::Value(ReviewConfig {
                native_specialists: vec![ReviewSpecialistConfig {
                    agent_key: "reviewer-ux".into(),
                    nickname: "Noether".into(),
                    role: "ux_review".into(),
                    focus: "TUI UX and tmux evidence".into(),
                }],
            }),
            ..Default::default()
        });

        let review = config.review.as_ref().expect("review config set");
        assert_eq!(review.native_specialists.len(), 1);
        assert_eq!(review.native_specialists[0].agent_key, "reviewer-ux");
    }

    #[test]
    fn test_profile_config_patch_clears_structured_llm_contract() {
        let mut config = ProfileConfig {
            llm: Some(llm_profile(
                llm_selection("openai", "gpt-4.1", None, None),
                vec![],
            )),
            ..Default::default()
        };

        config.apply_patch(ProfileConfigPatch {
            llm: PatchField::Clear,
            ..Default::default()
        });

        assert!(config.llm.is_none());
        assert!(!config.has_llm_selection());
    }

    #[test]
    fn test_profile_config_patch_replaces_structured_llm_contract() {
        let mut config = ProfileConfig {
            llm: Some(llm_profile(
                llm_selection("openai", "gpt-4.1", None, None),
                vec![],
            )),
            ..Default::default()
        };

        config.apply_patch(ProfileConfigPatch {
            llm: PatchField::Value(llm_profile(
                llm_selection("moonshot", "kimi-k2.5", Some("MOONSHOT_API_KEY"), None),
                vec![],
            )),
            ..Default::default()
        });

        let primary = config
            .llm
            .as_ref()
            .and_then(|llm| llm.primary.as_ref())
            .expect("rebuilt primary selection");
        assert_eq!(primary.family_id.as_deref(), Some("moonshot"));
        assert_eq!(primary.model_id.as_deref(), Some("kimi-k2.5"));
        assert_eq!(
            primary
                .route
                .as_ref()
                .and_then(|route| route.api_key_env.as_deref()),
            Some("MOONSHOT_API_KEY")
        );
    }

    #[test]
    fn test_config_from_profile_bridge_url_override() {
        let profile = UserProfile {
            id: "wa-test".into(),
            name: "WA Test".into(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                llm: Some(llm_profile(
                    llm_selection("anthropic", "claude-sonnet-4-20250514", None, None),
                    vec![],
                )),
                channels: vec![ChannelCredentials::WhatsApp {
                    bridge_url: "ws://localhost:3001".into(),
                }],
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        // Without override: uses original bridge_url
        let config = config_from_profile(&profile, None, None);
        let gw = config.gateway.as_ref().unwrap();
        assert_eq!(gw.channels[0].settings["bridge_url"], "ws://localhost:3001");

        // With override: uses managed bridge URL
        let config = config_from_profile(&profile, Some("ws://localhost:3105"), None);
        let gw = config.gateway.as_ref().unwrap();
        assert_eq!(gw.channels[0].settings["bridge_url"], "ws://localhost:3105");
    }

    #[test]
    fn test_resolve_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::open_unified(dir.path()).unwrap();

        let mut profile = UserProfile {
            id: "alice".into(),
            name: "Alice".into(),
            enabled: false,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig::default(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        // Default: profiles_dir/{id}/data
        let default_dir = store.resolve_data_dir(&profile);
        assert!(default_dir.ends_with("alice/data"));

        // Override
        profile.data_dir = Some("/custom/path".into());
        let custom_dir = store.resolve_data_dir(&profile);
        assert_eq!(custom_dir, PathBuf::from("/custom/path"));
    }

    #[test]
    fn should_read_registry_from_config_root_and_data_from_data_root() {
        // Multi-instance stdio: a split store reads/writes the `<id>.json`
        // REGISTRY under the SHARED registry_root while the per-profile
        // `<id>/data` runtime tree roots under the PER-INSTANCE data_root.
        let registry_root = tempfile::tempdir().unwrap();
        let data_root = tempfile::tempdir().unwrap();
        let store = ProfileStore::open(registry_root.path(), data_root.path()).unwrap();

        let profile = UserProfile {
            id: "alice".into(),
            name: "Alice".into(),
            enabled: false,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig::default(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        store.save(&profile).unwrap();

        // Registry json lands under registry_root/profiles — NOT data_root.
        let registry_json = registry_root.path().join("profiles").join("alice.json");
        assert!(
            registry_json.exists(),
            "registry <id>.json must live under registry_root/profiles"
        );
        assert!(
            !data_root
                .path()
                .join("profiles")
                .join("alice.json")
                .exists(),
            "registry <id>.json must NOT live under data_root/profiles"
        );
        // The store must be able to read the profile back through the registry.
        assert!(store.get("alice").unwrap().is_some());
        assert_eq!(store.list().unwrap().len(), 1);

        // Per-profile data roots under data_root/profiles — NOT registry_root.
        let resolved = store.resolve_data_dir(&profile);
        assert!(
            resolved.starts_with(data_root.path().join("profiles")),
            "resolve_data_dir must root under data_root/profiles, got {}",
            resolved.display()
        );
        assert!(resolved.ends_with("alice/data"));

        // octos_home_dir points at the REGISTRY root (shared, config-like).
        assert_eq!(store.octos_home_dir(), registry_root.path());

        // Regression guard: open_unified(x) collapses both roots under x.
        let unified_dir = tempfile::tempdir().unwrap();
        let unified = ProfileStore::open_unified(unified_dir.path()).unwrap();
        unified.save(&profile).unwrap();
        assert!(
            unified_dir
                .path()
                .join("profiles")
                .join("alice.json")
                .exists(),
            "open_unified registry must live under x/profiles"
        );
        assert!(
            unified
                .resolve_data_dir(&profile)
                .starts_with(unified_dir.path().join("profiles")),
            "open_unified data must root under x/profiles"
        );
    }

    #[test]
    fn test_mask_secrets() {
        assert_eq!(mask_value(""), "");
        assert_eq!(mask_value("short"), "***");
        assert_eq!(mask_value("exactly12ch"), "***");
        assert_eq!(mask_value("sk-1234567890abcdef"), "sk-1***def");

        let profile = UserProfile {
            id: "test".into(),
            name: "Test".into(),
            enabled: false,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                env_vars: [
                    ("API_KEY".into(), "sk-1234567890abcdef".into()),
                    ("SHORT".into(), "abc".into()),
                ]
                .into(),
                channels: vec![
                    ChannelCredentials::Api {
                        port: 9911,
                        auth_token: Some("api-token-secret".into()),
                    },
                    ChannelCredentials::Matrix {
                        homeserver: "https://matrix.example.org".into(),
                        as_token: "as-token-secret".into(),
                        hs_token: "hs-token-secret".into(),
                        server_name: "example.org".into(),
                        sender_localpart: "octos".into(),
                        user_prefix: "octos_".into(),
                        port: 8009,
                        allowed_senders: Vec::new(),
                        mention_only: true,
                        mode: "user".into(),
                        user_id: "@bot:example.org".into(),
                        access_token: "syt_access_token_secret".into(),
                        password: "matrix-password".into(),
                        device_name: "octos".into(),
                        rooms: Vec::new(),
                        auto_join: "off".into(),
                        auto_join_allowlist: Vec::new(),
                        group_policy: "allowlist".into(),
                        require_mention: true,
                    },
                ],
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let masked = mask_secrets(&profile);
        assert_eq!(masked.config.env_vars["API_KEY"], "sk-1***def");
        assert_eq!(masked.config.env_vars["SHORT"], "***");
        let ChannelCredentials::Api { auth_token, .. } = &masked.config.channels[0] else {
            panic!("expected api channel");
        };
        assert_eq!(auth_token.as_deref(), Some("api-***ret"));
        let ChannelCredentials::Matrix {
            as_token,
            hs_token,
            access_token,
            password,
            ..
        } = &masked.config.channels[1]
        else {
            panic!("expected matrix channel");
        };
        assert_eq!(as_token, "as-t***ret");
        assert_eq!(hs_token, "hs-t***ret");
        assert_eq!(access_token, "syt_***ret");
        assert_eq!(password, "matr***ord");
    }

    #[test]
    fn test_file_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::open_unified(dir.path()).unwrap();
        let profile = UserProfile {
            id: "perms-test".into(),
            name: "Perms".into(),
            enabled: false,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig::default(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        store.save(&profile).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = std::fs::metadata(store.profile_path("perms-test")).unwrap();
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn test_save_with_merge_preserves_masked_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::open_unified(dir.path()).unwrap();

        // Save a profile with real secrets
        let original = UserProfile {
            id: "merge-test".into(),
            name: "Merge".into(),
            enabled: false,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                env_vars: [
                    ("API_KEY".into(), "sk-real-secret-key".into()),
                    ("OTHER".into(), "value-to-keep".into()),
                ]
                .into(),
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        store.save(&original).unwrap();

        // Simulate update with masked values and a new value
        let mut updated = UserProfile {
            id: "merge-test".into(),
            name: "Merge".into(),
            enabled: false,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                env_vars: [
                    ("API_KEY".into(), "sk-r***key".into()), // masked — should keep original
                    ("OTHER".into(), "new-value".into()),    // changed — should update
                    ("NEW_KEY".into(), "brand-new".into()),  // new — should add
                ]
                .into(),
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        store.save_with_merge(&mut updated).unwrap();

        let loaded = store.get("merge-test").unwrap().unwrap();
        assert_eq!(loaded.config.env_vars["API_KEY"], "sk-real-secret-key");
        assert_eq!(loaded.config.env_vars["OTHER"], "new-value");
        assert_eq!(loaded.config.env_vars["NEW_KEY"], "brand-new");
    }

    /// Build a profile whose `config.email` carries both literal secrets.
    fn email_secret_profile(id: &str) -> UserProfile {
        UserProfile {
            id: id.into(),
            name: "Email Secrets".into(),
            enabled: false,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                email: Some(EmailSettings {
                    provider: "smtp".into(),
                    smtp_host: Some("smtp.example.org".into()),
                    smtp_port: Some(587),
                    username: Some("bot@example.org".into()),
                    password_env: Some("SMTP_PASSWORD".into()),
                    password: Some("real-smtp-password".into()),
                    from_address: Some("bot@example.org".into()),
                    feishu_app_id: Some("cli_realappid".into()),
                    feishu_app_secret_env: Some("FEISHU_APP_SECRET".into()),
                    feishu_app_secret: Some("real-feishu-app-secret".into()),
                    feishu_from_address: None,
                    feishu_region: None,
                }),
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn test_mask_secrets_masks_email_password_and_feishu_secret() {
        let masked = mask_secrets(&email_secret_profile("email-mask"));
        let email = masked.config.email.expect("email settings survive masking");

        // The literal credentials must never reach the wire.
        assert!(is_display_secret_value(email.password.as_deref().unwrap()));
        assert!(is_display_secret_value(
            email.feishu_app_secret.as_deref().unwrap()
        ));
        assert_ne!(email.password.as_deref(), Some("real-smtp-password"));
        assert_ne!(
            email.feishu_app_secret.as_deref(),
            Some("real-feishu-app-secret")
        );

        // The `*_env` twins name env vars, not secrets — they stay readable, or
        // the settings page cannot tell you which variable to set.
        assert_eq!(email.password_env.as_deref(), Some("SMTP_PASSWORD"));
        assert_eq!(
            email.feishu_app_secret_env.as_deref(),
            Some("FEISHU_APP_SECRET")
        );
        assert_eq!(email.username.as_deref(), Some("bot@example.org"));
    }

    #[test]
    fn test_save_with_merge_preserves_masked_email_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::open_unified(dir.path()).unwrap();
        store.save(&email_secret_profile("email-merge")).unwrap();

        // A client GETs the masked profile and PUTs it straight back.
        let mut round_tripped = mask_secrets(&store.get("email-merge").unwrap().unwrap());
        store.save_with_merge(&mut round_tripped).unwrap();

        let loaded = store.get("email-merge").unwrap().unwrap();
        let email = loaded
            .config
            .email
            .expect("email settings survive the merge");
        assert_eq!(email.password.as_deref(), Some("real-smtp-password"));
        assert_eq!(
            email.feishu_app_secret.as_deref(),
            Some("real-feishu-app-secret")
        );
    }

    /// Build a profile whose `config.smart_home` carries a literal token.
    fn smart_home_secret_profile(id: &str) -> UserProfile {
        UserProfile {
            id: id.into(),
            name: "Smart Home Secrets".into(),
            enabled: false,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                smart_home: Some(SmartHomeConfig {
                    bridge_url: Some("http://192.168.1.50:8787".into()),
                    token: Some("real-bridge-token".into()),
                    token_env: None,
                }),
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn test_save_with_merge_preserves_masked_smart_home_token() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::open_unified(dir.path()).unwrap();
        store.save(&smart_home_secret_profile("sh-merge")).unwrap();

        // A client GETs the masked profile and PUTs it straight back — the
        // settings page does exactly this for every unrelated config save.
        let mut round_tripped = mask_secrets(&store.get("sh-merge").unwrap().unwrap());
        store.save_with_merge(&mut round_tripped).unwrap();

        let loaded = store.get("sh-merge").unwrap().unwrap();
        let smart_home = loaded
            .config
            .smart_home
            .expect("smart_home settings survive the merge");
        assert_eq!(smart_home.token.as_deref(), Some("real-bridge-token"));
        assert_eq!(
            smart_home.bridge_url.as_deref(),
            Some("http://192.168.1.50:8787")
        );
    }

    #[test]
    fn test_save_with_merge_allows_changing_smart_home_token() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::open_unified(dir.path()).unwrap();
        store.save(&smart_home_secret_profile("sh-change")).unwrap();

        // A genuinely new value is NOT a display artifact, so it must land.
        let mut updated = smart_home_secret_profile("sh-change");
        updated.config.smart_home.as_mut().unwrap().token = Some("rotated-bridge-token".into());
        store.save_with_merge(&mut updated).unwrap();

        let loaded = store.get("sh-change").unwrap().unwrap();
        let smart_home = loaded.config.smart_home.unwrap();
        assert_eq!(smart_home.token.as_deref(), Some("rotated-bridge-token"));
    }

    #[test]
    fn test_save_with_merge_allows_changing_email_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::open_unified(dir.path()).unwrap();
        store.save(&email_secret_profile("email-change")).unwrap();

        // A genuinely new value is NOT a display artifact, so it must land.
        let mut updated = email_secret_profile("email-change");
        let email = updated.config.email.as_mut().unwrap();
        email.password = Some("rotated-smtp-password".into());
        email.feishu_app_secret = Some("rotated-feishu-secret".into());
        store.save_with_merge(&mut updated).unwrap();

        let loaded = store.get("email-change").unwrap().unwrap();
        let email = loaded.config.email.unwrap();
        assert_eq!(email.password.as_deref(), Some("rotated-smtp-password"));
        assert_eq!(
            email.feishu_app_secret.as_deref(),
            Some("rotated-feishu-secret")
        );
    }

    #[test]
    fn test_save_with_merge_preserves_masked_channel_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::open_unified(dir.path()).unwrap();

        let original = UserProfile {
            id: "channel-merge".into(),
            name: "Channel Merge".into(),
            enabled: false,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                channels: vec![
                    ChannelCredentials::Api {
                        port: 9911,
                        auth_token: Some("api-real-token".into()),
                    },
                    ChannelCredentials::Matrix {
                        homeserver: "https://old.example.org".into(),
                        as_token: "as-real-token".into(),
                        hs_token: "hs-real-token".into(),
                        server_name: "old.example.org".into(),
                        sender_localpart: "octos".into(),
                        user_prefix: "octos_".into(),
                        port: 8009,
                        allowed_senders: Vec::new(),
                        mention_only: true,
                        mode: "user".into(),
                        user_id: "@bot:old.example.org".into(),
                        access_token: "syt_real_access_token".into(),
                        password: "real-password".into(),
                        device_name: "old-device".into(),
                        rooms: Vec::new(),
                        auto_join: "off".into(),
                        auto_join_allowlist: Vec::new(),
                        group_policy: "allowlist".into(),
                        require_mention: true,
                    },
                ],
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        store.save(&original).unwrap();

        let mut updated = original.clone();
        let ChannelCredentials::Api { auth_token, .. } = &mut updated.config.channels[0] else {
            panic!("expected api channel");
        };
        *auth_token = Some("api-***ken".into());
        let ChannelCredentials::Matrix {
            homeserver,
            as_token,
            hs_token,
            access_token,
            password,
            device_name,
            ..
        } = &mut updated.config.channels[1]
        else {
            panic!("expected matrix channel");
        };
        *homeserver = "https://new.example.org".into();
        *as_token = "as-r***ken".into();
        *hs_token = "hs-r***ken".into();
        *access_token = "syt_***ken".into();
        *password = "***".into();
        *device_name = "new-device".into();
        store.save_with_merge(&mut updated).unwrap();

        let loaded = store.get("channel-merge").unwrap().unwrap();
        let ChannelCredentials::Api { auth_token, .. } = &loaded.config.channels[0] else {
            panic!("expected api channel");
        };
        assert_eq!(auth_token.as_deref(), Some("api-real-token"));
        let ChannelCredentials::Matrix {
            homeserver,
            as_token,
            hs_token,
            access_token,
            password,
            device_name,
            ..
        } = &loaded.config.channels[1]
        else {
            panic!("expected matrix channel");
        };
        assert_eq!(homeserver, "https://new.example.org");
        assert_eq!(as_token, "as-real-token");
        assert_eq!(hs_token, "hs-real-token");
        assert_eq!(access_token, "syt_real_access_token");
        assert_eq!(password, "real-password");
        assert_eq!(device_name, "new-device");
    }

    #[test]
    fn test_save_with_merge_allows_clearing_channel_secret() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::open_unified(dir.path()).unwrap();

        let original = UserProfile {
            id: "channel-clear".into(),
            name: "Channel Clear".into(),
            enabled: false,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                channels: vec![ChannelCredentials::Matrix {
                    homeserver: "https://matrix.example.org".into(),
                    as_token: String::new(),
                    hs_token: String::new(),
                    server_name: String::new(),
                    sender_localpart: "octos".into(),
                    user_prefix: "octos_".into(),
                    port: 8009,
                    allowed_senders: Vec::new(),
                    mention_only: true,
                    mode: "user".into(),
                    user_id: "@bot:example.org".into(),
                    access_token: "syt_old_access_token".into(),
                    password: "old-password".into(),
                    device_name: "octos".into(),
                    rooms: Vec::new(),
                    auto_join: "off".into(),
                    auto_join_allowlist: Vec::new(),
                    group_policy: "allowlist".into(),
                    require_mention: true,
                }],
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        store.save(&original).unwrap();

        let mut updated = original.clone();
        let ChannelCredentials::Matrix {
            access_token,
            password,
            ..
        } = &mut updated.config.channels[0]
        else {
            panic!("expected matrix channel");
        };
        *access_token = String::new();
        *password = "new-password".into();
        store.save_with_merge(&mut updated).unwrap();

        let loaded = store.get("channel-clear").unwrap().unwrap();
        let ChannelCredentials::Matrix {
            access_token,
            password,
            ..
        } = &loaded.config.channels[0]
        else {
            panic!("expected matrix channel");
        };
        assert_eq!(access_token, "");
        assert_eq!(password, "new-password");
    }

    #[test]
    fn test_save_with_merge_preserves_channel_secret_after_delete() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::open_unified(dir.path()).unwrap();

        let matrix_channel = |user_id: &str, token: &str| ChannelCredentials::Matrix {
            homeserver: "https://matrix.example.org".into(),
            as_token: String::new(),
            hs_token: String::new(),
            server_name: String::new(),
            sender_localpart: "octos".into(),
            user_prefix: "octos_".into(),
            port: 8009,
            allowed_senders: Vec::new(),
            mention_only: true,
            mode: "user".into(),
            user_id: user_id.into(),
            access_token: token.into(),
            password: String::new(),
            device_name: "octos".into(),
            rooms: Vec::new(),
            auto_join: "off".into(),
            auto_join_allowlist: Vec::new(),
            group_policy: "allowlist".into(),
            require_mention: true,
        };

        let original = UserProfile {
            id: "channel-delete".into(),
            name: "Channel Delete".into(),
            enabled: false,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                channels: vec![
                    matrix_channel("@first:example.org", "syt_first_token"),
                    matrix_channel("@second:example.org", "syt_second_token"),
                ],
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        store.save(&original).unwrap();

        let mut updated = original.clone();
        updated.config.channels.remove(0);
        let ChannelCredentials::Matrix { access_token, .. } = &mut updated.config.channels[0]
        else {
            panic!("expected matrix channel");
        };
        *access_token = "syt_***ken".into();
        store.save_with_merge(&mut updated).unwrap();

        let loaded = store.get("channel-delete").unwrap().unwrap();
        assert_eq!(loaded.config.channels.len(), 1);
        let ChannelCredentials::Matrix {
            user_id,
            access_token,
            ..
        } = &loaded.config.channels[0]
        else {
            panic!("expected matrix channel");
        };
        assert_eq!(user_id, "@second:example.org");
        assert_eq!(access_token, "syt_second_token");
    }

    #[test]
    fn test_diff_profiles_model_change_is_hot() {
        let base = UserProfile {
            id: "diff-test".into(),
            name: "Diff".into(),
            enabled: false,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                llm: Some(llm_profile(
                    llm_selection("openai", "gpt-4o", None, None),
                    vec![],
                )),
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let mut changed = base.clone();
        changed.config.llm = Some(llm_profile(
            llm_selection("openai", "gpt-4o-mini", None, None),
            vec![],
        ));

        assert!(matches!(
            diff_profiles(&base, &changed),
            ProfileChange::RestartRequired(fields) if fields == vec!["llm"]
        ));
    }

    #[test]
    fn test_diff_profiles_hot_reloadable() {
        let base = UserProfile {
            id: "diff-test".into(),
            name: "Diff".into(),
            enabled: false,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                llm: Some(llm_profile(
                    llm_selection("openai", "gpt-4o", None, None),
                    vec![],
                )),
                gateway: GatewaySettings {
                    system_prompt: Some("old".into()),
                    ..Default::default()
                },
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let mut changed = base.clone();
        changed.config.gateway.system_prompt = Some("new".into());

        assert!(matches!(
            diff_profiles(&base, &changed),
            ProfileChange::HotReloadable
        ));
    }

    #[test]
    fn test_diff_profiles_structured_sections_require_restart() {
        let base = UserProfile {
            id: "diff-test".into(),
            name: "Diff".into(),
            enabled: false,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                search: Some(SearchConfig {
                    providers: [(
                        "tavily".into(),
                        SearchProviderConfig {
                            api_key_env: Some("TAVILY_PARENT".into()),
                        },
                    )]
                    .into(),
                }),
                deep_crawl: Some(DeepCrawlConfig {
                    page_settle_ms: Some(1500),
                    max_output_chars: Some(32_000),
                }),
                apps: Some(AppsConfig {
                    slides: Some(SlidesAppConfig {
                        template_dir: Some("/opt/octos/slides".into()),
                        default_theme: Some("crew".into()),
                    }),
                }),
                review: Some(ReviewConfig {
                    native_specialists: vec![ReviewSpecialistConfig {
                        agent_key: "reviewer-api".into(),
                        nickname: "Ada Lovelace".into(),
                        role: "api_contract_review".into(),
                        focus: "API".into(),
                    }],
                }),
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let mut changed = base.clone();
        changed.config.search = Some(SearchConfig {
            providers: [(
                "tavily".into(),
                SearchProviderConfig {
                    api_key_env: Some("TAVILY_CHILD".into()),
                },
            )]
            .into(),
        });
        changed.config.deep_crawl = Some(DeepCrawlConfig {
            page_settle_ms: Some(2500),
            max_output_chars: Some(48_000),
        });
        changed.config.apps = Some(AppsConfig {
            slides: Some(SlidesAppConfig {
                template_dir: Some("/srv/slides".into()),
                default_theme: Some("ocean".into()),
            }),
        });
        changed.config.review = Some(ReviewConfig {
            native_specialists: vec![ReviewSpecialistConfig {
                agent_key: "reviewer-ux".into(),
                nickname: "Noether".into(),
                role: "ux_review".into(),
                focus: "TUI UX".into(),
            }],
        });

        match diff_profiles(&base, &changed) {
            ProfileChange::RestartRequired(fields) => {
                assert!(fields.contains(&"review".into()));
                assert!(fields.contains(&"search".into()));
                assert!(fields.contains(&"deep_crawl".into()));
                assert!(fields.contains(&"apps".into()));
            }
            other => panic!("expected RestartRequired, got {other:?}"),
        }
    }

    #[test]
    fn should_classify_realtime_config_as_restart_required() {
        let base = UserProfile {
            id: "rp05-diff".into(),
            name: "RP05".into(),
            enabled: false,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                robot: Some(RobotConfig {
                    realtime: Some(octos_agent::RealtimeConfig {
                        enabled: false,
                        ..Default::default()
                    }),
                }),
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let mut changed = base.clone();
        changed.config.robot = Some(RobotConfig {
            realtime: Some(octos_agent::RealtimeConfig {
                enabled: true,
                heartbeat_timeout_ms: 250,
                ..Default::default()
            }),
        });

        match diff_profiles(&base, &changed) {
            ProfileChange::RestartRequired(fields) => {
                assert!(
                    fields.iter().any(|f| f == "robot"),
                    "expected `robot` in restart-required fields, got {fields:?}",
                );
            }
            other => panic!("expected RestartRequired, got {other:?}"),
        }
    }

    #[test]
    fn should_classify_credential_pool_as_restart_required() {
        let base = UserProfile {
            id: "m65-diff".into(),
            name: "M6.5".into(),
            enabled: false,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                credential_pool: Some(CredentialPoolConfig {
                    schema_version: 1,
                    pools: [(
                        "anthropic".into(),
                        CredentialPoolEntry {
                            strategy: "round_robin".into(),
                            credential_ids: vec!["k1".into(), "k2".into()],
                            ..Default::default()
                        },
                    )]
                    .into(),
                }),
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let mut changed = base.clone();
        changed.config.credential_pool = Some(CredentialPoolConfig {
            schema_version: 1,
            pools: [(
                "anthropic".into(),
                CredentialPoolEntry {
                    strategy: "fill_first".into(),
                    credential_ids: vec!["k1".into(), "k2".into(), "k3".into()],
                    ..Default::default()
                },
            )]
            .into(),
        });

        match diff_profiles(&base, &changed) {
            ProfileChange::RestartRequired(fields) => {
                assert!(
                    fields.iter().any(|f| f == "credential_pool"),
                    "expected `credential_pool` in restart-required fields, got {fields:?}",
                );
            }
            other => panic!("expected RestartRequired, got {other:?}"),
        }
    }

    #[test]
    fn should_classify_runtime_policy_config_as_restart_required() {
        let base = UserProfile {
            id: "runtime-policy-diff".into(),
            name: "Runtime Policy".into(),
            enabled: false,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig::default(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let mut lane_routing = octos_llm::LaneRoutingConfig::default();
        lane_routing
            .topic_lanes
            .insert("code".into(), octos_llm::Lane::CodeCapable);

        let mut changed = base.clone();
        changed.config.admin_mode = true;
        changed.config.sandbox.allow_network = true;
        changed.config.adaptive_routing = Some(crate::config::AdaptiveRoutingConfig {
            enabled: true,
            ..Default::default()
        });
        changed.config.content_routing = Some(octos_llm::RoutingConfig {
            enabled: true,
            ..Default::default()
        });
        changed.config.plugins.require_signed = true;
        changed.config.lane_routing = Some(lane_routing);

        match diff_profiles(&base, &changed) {
            ProfileChange::RestartRequired(fields) => {
                for field in [
                    "admin_mode",
                    "sandbox",
                    "adaptive_routing",
                    "content_routing",
                    "plugins",
                    "lane_routing",
                ] {
                    assert!(
                        fields.iter().any(|candidate| candidate == field),
                        "expected `{field}` in restart-required fields, got {fields:?}",
                    );
                }
            }
            other => panic!("expected RestartRequired, got {other:?}"),
        }
    }

    #[test]
    fn should_default_credential_pool_config_schema_version() {
        let cfg = CredentialPoolConfig::default();
        assert_eq!(cfg.schema_version, 1);
        assert!(cfg.pools.is_empty());

        // Deserialization backfills the schema version.
        let raw = serde_json::json!({
            "pools": {
                "openai": {
                    "credential_ids": ["a", "b"]
                }
            }
        });
        let parsed: CredentialPoolConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(parsed.schema_version, 1);
        assert_eq!(parsed.pools.len(), 1);
        let p = &parsed.pools["openai"];
        assert_eq!(p.strategy, "round_robin");
        assert_eq!(p.credential_ids, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn test_diff_profiles_unchanged() {
        let base = UserProfile {
            id: "diff-test".into(),
            name: "Diff".into(),
            enabled: false,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig::default(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        // Only name changed (not config) — should be Unchanged
        let mut changed = base.clone();
        changed.name = "New Name".into();

        assert!(matches!(
            diff_profiles(&base, &changed),
            ProfileChange::Unchanged
        ));
    }

    #[test]
    fn test_create_sub_account() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::open_unified(dir.path()).unwrap();

        // Create parent with LLM config
        let parent = UserProfile {
            id: "parent".into(),
            name: "Parent Bot".into(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                llm: Some(llm_profile(
                    llm_selection("openai", "gpt-4o", Some("OPENAI_API_KEY"), None),
                    vec![],
                )),
                env_vars: [("OPENAI_API_KEY".into(), "sk-test-key".into())].into(),
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        store.save(&parent).unwrap();

        // Create sub-account
        let sub = store
            .create_sub_account(
                "parent",
                "work-bot",
                "work-bot",
                "work bot",
                vec![ChannelCredentials::Telegram {
                    token_env: "WORK_TG_TOKEN".into(),
                    allowed_senders: String::new(),
                }],
                GatewaySettings::default(),
            )
            .unwrap();

        assert_eq!(sub.id, "parent--work-bot");
        assert_eq!(sub.parent_id, Some("parent".into()));
        assert!(sub.config.llm.is_none()); // Not set — inherited at runtime
        assert_eq!(sub.config.channels.len(), 1);

        // List sub-accounts
        let subs = store.list_sub_accounts("parent").unwrap();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].id, "parent--work-bot");

        // No sub-accounts for non-existent parent
        let empty = store.list_sub_accounts("nonexistent").unwrap();
        assert!(empty.is_empty());

        // Duplicate should fail
        assert!(
            store
                .create_sub_account(
                    "parent",
                    "work-bot",
                    "work-bot",
                    "work bot",
                    vec![],
                    GatewaySettings::default(),
                )
                .is_err()
        );
    }

    #[test]
    fn test_public_subdomain_must_be_unique() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::open_unified(dir.path()).unwrap();

        let first = UserProfile {
            id: "top-level".into(),
            name: "Top".into(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: Some("shared-host".into()),
            config: ProfileConfig::default(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let second = UserProfile {
            id: "top-level-2".into(),
            name: "Top 2".into(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: Some("shared-host".into()),
            config: ProfileConfig::default(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        store.save(&first).unwrap();
        assert!(store.save(&second).is_err());
    }

    #[test]
    fn test_resolve_routable_profile_id_prefers_public_subdomain() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::open_unified(dir.path()).unwrap();

        let parent = UserProfile {
            id: "tenant".into(),
            name: "Tenant".into(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig::default(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let child = UserProfile {
            id: "tenant--newsbot".into(),
            name: "Newsbot".into(),
            enabled: true,
            data_dir: None,
            parent_id: Some("tenant".into()),
            public_subdomain: Some("newsbot".into()),
            config: ProfileConfig::default(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        store.save(&parent).unwrap();
        store.save(&child).unwrap();

        assert_eq!(
            store
                .resolve_routable_profile_id("newsbot")
                .unwrap()
                .as_deref(),
            Some("tenant--newsbot")
        );
        assert_eq!(
            store
                .resolve_routable_profile_id("tenant")
                .unwrap()
                .as_deref(),
            Some("tenant")
        );
        assert!(
            store
                .resolve_routable_profile_id("tenant--newsbot")
                .unwrap()
                .is_none(),
            "child internal IDs must not be routable once public_subdomain is authoritative"
        );
    }

    #[test]
    fn test_resolve_effective_profile() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::open_unified(dir.path()).unwrap();

        // Create parent
        let parent = UserProfile {
            id: "parent".into(),
            name: "Parent".into(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                llm: Some(llm_profile(
                    llm_selection(
                        "openai",
                        "gpt-4o",
                        Some("OPENAI_API_KEY"),
                        Some("https://custom.api.com/v1"),
                    ),
                    vec![llm_selection(
                        "anthropic",
                        "claude-sonnet-4-20250514",
                        None,
                        None,
                    )],
                )),
                env_vars: [
                    ("OPENAI_API_KEY".into(), "sk-parent-key".into()),
                    ("SHARED_VAR".into(), "parent-value".into()),
                ]
                .into(),
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        store.save(&parent).unwrap();

        // Create sub-account with own channel and env var
        let sub = UserProfile {
            id: "parent--work".into(),
            name: "Work".into(),
            enabled: false,
            data_dir: None,
            parent_id: Some("parent".into()),
            public_subdomain: Some("work".into()),
            config: ProfileConfig {
                channels: vec![ChannelCredentials::Telegram {
                    token_env: "WORK_TG".into(),
                    allowed_senders: String::new(),
                }],
                env_vars: [
                    ("WORK_TG".into(), "work-token".into()),
                    ("SHARED_VAR".into(), "sub-override".into()), // overrides parent
                ]
                .into(),
                gateway: GatewaySettings {
                    system_prompt: Some("You are a work assistant.".into()),
                    ..Default::default()
                },
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        store.save(&sub).unwrap();

        let effective = resolve_effective_profile(&store, &sub).unwrap();

        // Inherited from parent
        assert_eq!(effective.config.primary_provider(), Some("openai"));
        assert_eq!(effective.config.primary_model(), Some("gpt-4o"));
        assert_eq!(
            effective
                .config
                .primary_llm()
                .and_then(|selection| selection.route.as_ref())
                .and_then(|route| route.base_url.as_deref()),
            Some("https://custom.api.com/v1")
        );
        assert_eq!(
            effective.config.llm.as_ref().map(|llm| llm.fallbacks.len()),
            Some(1)
        );

        // Sub-account's own settings preserved
        assert_eq!(effective.config.channels.len(), 1);
        assert_eq!(
            effective.config.gateway.system_prompt.as_deref(),
            Some("You are a work assistant.")
        );

        // Env vars merged: parent base + sub overrides
        assert_eq!(effective.config.env_vars["OPENAI_API_KEY"], "sk-parent-key");
        assert_eq!(effective.config.env_vars["WORK_TG"], "work-token");
        assert_eq!(effective.config.env_vars["SHARED_VAR"], "sub-override"); // sub wins

        // Top-level profile returns as-is
        let effective_parent = resolve_effective_profile(&store, &parent).unwrap();
        assert_eq!(effective_parent.id, "parent");
        assert_eq!(effective_parent.config.primary_provider(), Some("openai"));
    }

    #[test]
    fn test_resolve_effective_profile_inherits_structured_sections() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::open_unified(dir.path()).unwrap();

        let parent = UserProfile {
            id: "parent".into(),
            name: "Parent".into(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                search: Some(SearchConfig {
                    providers: [(
                        "brave".into(),
                        SearchProviderConfig {
                            api_key_env: Some("BRAVE_API_KEY".into()),
                        },
                    )]
                    .into(),
                }),
                deep_crawl: Some(DeepCrawlConfig {
                    page_settle_ms: Some(2_000),
                    max_output_chars: Some(12_000),
                }),
                apps: Some(AppsConfig {
                    slides: Some(SlidesAppConfig {
                        template_dir: Some("/srv/slides".into()),
                        default_theme: Some("operator".into()),
                    }),
                }),
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let child = UserProfile {
            id: "parent--child".into(),
            name: "Child".into(),
            enabled: true,
            data_dir: None,
            parent_id: Some("parent".into()),
            public_subdomain: Some("child".into()),
            config: ProfileConfig::default(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        store.save(&parent).unwrap();
        store.save(&child).unwrap();

        let effective = resolve_effective_profile(&store, &child).unwrap();
        assert_eq!(
            effective
                .config
                .search
                .as_ref()
                .and_then(|search| search.providers.get("brave"))
                .and_then(|provider| provider.api_key_env.as_deref()),
            Some("BRAVE_API_KEY")
        );
        assert_eq!(
            effective
                .config
                .deep_crawl
                .as_ref()
                .and_then(|cfg| cfg.max_output_chars),
            Some(12_000)
        );
        assert_eq!(
            effective
                .config
                .apps
                .as_ref()
                .and_then(|apps| apps.slides.as_ref())
                .and_then(|slides| slides.template_dir.as_deref()),
            Some("/srv/slides")
        );
    }

    #[test]
    fn test_diff_profiles_parent_id_change() {
        let base = UserProfile {
            id: "sub".into(),
            name: "Sub".into(),
            enabled: false,
            data_dir: None,
            parent_id: Some("parent-a".into()),
            public_subdomain: Some("sub".into()),
            config: ProfileConfig::default(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let mut changed = base.clone();
        changed.parent_id = Some("parent-b".into());

        match diff_profiles(&base, &changed) {
            ProfileChange::RestartRequired(fields) => {
                assert!(fields.contains(&"parent_id".into()));
            }
            other => panic!("expected RestartRequired, got {other:?}"),
        }
    }

    #[test]
    fn test_channel_serde_roundtrip() {
        let channels = vec![
            ChannelCredentials::Telegram {
                token_env: "TG".into(),
                allowed_senders: String::new(),
            },
            ChannelCredentials::Discord {
                token_env: "DC".into(),
            },
            ChannelCredentials::DingTalk {
                webhook_url_env: "DT_WEBHOOK".into(),
                secret_env: "DT_SECRET".into(),
                allowed_senders: "staff-1,staff-2".into(),
                webhook_port: Some(8650),
            },
            ChannelCredentials::Slack {
                bot_token_env: "SB".into(),
                app_token_env: "SA".into(),
            },
            ChannelCredentials::WhatsApp {
                bridge_url: "ws://localhost:3001".into(),
            },
            ChannelCredentials::Feishu {
                app_id_env: "FID".into(),
                app_secret_env: "FSE".into(),
                mode: String::new(),
                region: String::new(),
                webhook_port: None,
                verification_token_env: String::new(),
                encrypt_key_env: String::new(),
            },
            ChannelCredentials::Email {
                imap_host: "imap.test.com".into(),
                imap_port: 993,
                smtp_host: "smtp.test.com".into(),
                smtp_port: 465,
                username_env: "EU".into(),
                password_env: "EP".into(),
            },
        ];

        let json = serde_json::to_string(&channels).unwrap();
        let parsed: Vec<ChannelCredentials> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.len(), 7);
    }

    #[test]
    fn test_profile_config_patch_rejects_unknown_root_field() {
        let err = serde_json::from_value::<ProfileConfigPatch>(serde_json::json!({
            "gateway": { "max_history": 100 },
            "bogus": true
        }))
        .expect_err("unknown root field should be rejected");

        assert!(err.to_string().contains("unknown field `bogus`"));
    }

    #[test]
    fn test_profile_config_patch_rejects_unknown_gateway_field() {
        let err = serde_json::from_value::<ProfileConfigPatch>(serde_json::json!({
            "gateway": {
                "max_history": 100,
                "bogus": true
            }
        }))
        .expect_err("unknown gateway field should be rejected");

        assert!(err.to_string().contains("unknown field `bogus`"));
    }

    #[test]
    fn test_profile_config_patch_rejects_unknown_deep_crawl_field() {
        let err = serde_json::from_value::<ProfileConfigPatch>(serde_json::json!({
            "deep_crawl": {
                "page_settle_ms": 1000,
                "max_chrs": 32000
            }
        }))
        .expect_err("unknown deep_crawl field should be rejected");

        assert!(err.to_string().contains("unknown field `max_chrs`"));
    }

    #[test]
    fn test_profile_config_patch_rejects_unknown_llm_route_field() {
        let err = serde_json::from_value::<ProfileConfigPatch>(serde_json::json!({
            "llm": {
                "primary": {
                    "family_id": "moonshot",
                    "model_id": "kimi-k2.5",
                    "route": {
                        "route_id": "official",
                        "bogus": true
                    }
                }
            }
        }))
        .expect_err("unknown llm route field should be rejected");

        assert!(err.to_string().contains("unknown field `bogus`"));
    }

    // ── Keychain marker tests ──────────────────────────────────────────

    #[test]
    fn test_mask_secrets_keychain_marker() {
        let profile = UserProfile {
            id: "kc".into(),
            name: "KC".into(),
            enabled: false,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                env_vars: [
                    ("KC_KEY".into(), "keychain:".into()),
                    ("PLAIN_KEY".into(), "sk-1234567890abcdef".into()),
                    ("SHORT".into(), "abc".into()),
                    ("EMPTY".into(), String::new()),
                ]
                .into(),
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let masked = mask_secrets(&profile);
        assert_eq!(
            masked.config.env_vars["KC_KEY"], "\u{1f511} (keychain)",
            "keychain marker should display as key emoji"
        );
        assert_eq!(masked.config.env_vars["PLAIN_KEY"], "sk-1***def");
        assert_eq!(masked.config.env_vars["SHORT"], "***");
        assert_eq!(masked.config.env_vars["EMPTY"], "");
    }

    #[test]
    fn test_save_with_merge_preserves_keychain_marker() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::open_unified(dir.path()).unwrap();

        // Save profile with keychain marker
        let original = UserProfile {
            id: "kc-merge".into(),
            name: "KC Merge".into(),
            enabled: false,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                env_vars: [
                    ("API_KEY".into(), "keychain:".into()),
                    ("OTHER".into(), "plaintext-value".into()),
                ]
                .into(),
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        store.save(&original).unwrap();

        // Simulate dashboard PUT with masked keychain display value
        let mut updated = original.clone();
        updated
            .config
            .env_vars
            .insert("API_KEY".into(), "\u{1f511} (keychain)".into());
        updated
            .config
            .env_vars
            .insert("OTHER".into(), "plai***lue".into());
        store.save_with_merge(&mut updated).unwrap();

        let loaded = store.get("kc-merge").unwrap().unwrap();
        assert_eq!(
            loaded.config.env_vars["API_KEY"], "keychain:",
            "keychain marker must be preserved when dashboard sends masked form"
        );
        assert_eq!(
            loaded.config.env_vars["OTHER"], "plaintext-value",
            "masked plaintext value must be restored from existing"
        );
    }

    #[test]
    fn test_save_with_merge_allows_setting_keychain_marker() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::open_unified(dir.path()).unwrap();

        // Profile with plaintext secret
        let original = UserProfile {
            id: "kc-set".into(),
            name: "KC Set".into(),
            enabled: false,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                env_vars: [("API_KEY".into(), "sk-real-secret".into())].into(),
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        store.save(&original).unwrap();

        // Explicitly setting "keychain:" should NOT be treated as masked
        let mut updated = original.clone();
        updated
            .config
            .env_vars
            .insert("API_KEY".into(), "keychain:".into());
        store.save_with_merge(&mut updated).unwrap();

        let loaded = store.get("kc-set").unwrap().unwrap();
        assert_eq!(
            loaded.config.env_vars["API_KEY"], "keychain:",
            "explicit keychain: marker must be stored, not reverted to old value"
        );
    }

    #[test]
    fn test_save_with_merge_empty_does_not_overwrite_keychain() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::open_unified(dir.path()).unwrap();

        let original = UserProfile {
            id: "kc-empty".into(),
            name: "KC Empty".into(),
            enabled: false,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                env_vars: [("API_KEY".into(), "keychain:".into())].into(),
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        store.save(&original).unwrap();

        // Empty value should restore existing (keychain marker)
        let mut updated = original.clone();
        updated
            .config
            .env_vars
            .insert("API_KEY".into(), String::new());
        store.save_with_merge(&mut updated).unwrap();

        let loaded = store.get("kc-empty").unwrap().unwrap();
        assert_eq!(
            loaded.config.env_vars["API_KEY"], "keychain:",
            "empty value must not overwrite keychain marker"
        );
    }

    #[test]
    fn test_matrix_channel_credentials_roundtrip() {
        let channel: ChannelCredentials = serde_json::from_value(serde_json::json!({
            "type": "matrix",
            "homeserver": "http://localhost:6167",
            "as_token": "test-as-token",
            "hs_token": "test-hs-token",
            "server_name": "localhost"
        }))
        .unwrap();

        let json = serde_json::to_value(&channel).unwrap();
        assert_eq!(json["homeserver"], "http://localhost:6167");
        assert_eq!(json["as_token"], "test-as-token");
        assert_eq!(json["hs_token"], "test-hs-token");
        assert_eq!(json["server_name"], "localhost");
        assert_eq!(json["sender_localpart"], "bot");
        assert_eq!(json["user_prefix"], "bot_");
        assert_eq!(json["port"], 8009);
    }

    #[test]
    fn test_matrix_user_mode_channel_to_entry_emits_account_settings() {
        let channel: ChannelCredentials = serde_json::from_value(serde_json::json!({
            "type": "matrix",
            "mode": "user",
            "homeserver": "https://matrix.org",
            "access_token": "syt_token",
            "rooms": ["!a:matrix.org", "!b:matrix.org"],
            "auto_join": "allowlist",
            "auto_join_allowlist": ["!a:matrix.org"],
            "group_policy": "allowlist",
            "require_mention": true,
        }))
        .unwrap();

        let entry = channel_to_entry(&channel);
        assert_eq!(entry["type"], "matrix");
        let settings = &entry["settings"];
        assert_eq!(settings["mode"], "user");
        assert_eq!(settings["homeserver"], "https://matrix.org");
        assert_eq!(settings["access_token"], "syt_token");
        assert_eq!(settings["rooms"][0], "!a:matrix.org");
        assert_eq!(settings["auto_join"], "allowlist");
        assert_eq!(settings["auto_join_allowlist"][0], "!a:matrix.org");
        assert_eq!(settings["group_policy"], "allowlist");
        assert_eq!(settings["require_mention"], true);
        // Appservice-only keys must not leak into a user-mode entry.
        assert!(settings.get("as_token").is_none());
        assert!(settings.get("hs_token").is_none());
        assert!(settings.get("port").is_none());
    }

    #[test]
    fn test_matrix_appservice_channel_to_entry_carries_mention_only() {
        // The documented opt-out (`mention_only: false`) must survive the
        // profiles → ChannelEntry conversion; otherwise a matrix channel
        // configured through this path silently reverts to the default.
        let channel: ChannelCredentials = serde_json::from_value(serde_json::json!({
            "type": "matrix",
            "homeserver": "http://localhost:6167",
            "as_token": "as",
            "hs_token": "hs",
            "server_name": "localhost",
            "mention_only": false,
        }))
        .unwrap();

        let entry = channel_to_entry(&channel);
        assert_eq!(entry["settings"]["mention_only"], false);

        // Omitted → safe default `true`, and the entry says so explicitly.
        let default_channel: ChannelCredentials = serde_json::from_value(serde_json::json!({
            "type": "matrix",
            "homeserver": "http://localhost:6167",
            "as_token": "as",
            "hs_token": "hs",
            "server_name": "localhost",
        }))
        .unwrap();
        assert_eq!(
            channel_to_entry(&default_channel)["settings"]["mention_only"],
            true
        );
    }

    #[test]
    fn test_matrix_user_mode_password_login_deserializes_without_appservice_tokens() {
        // A user-mode entry omits as_token/hs_token entirely; this must still
        // deserialize (regression guard for the now-optional appservice fields).
        let channel: ChannelCredentials = serde_json::from_value(serde_json::json!({
            "type": "matrix",
            "mode": "user",
            "homeserver": "https://matrix.org",
            "user_id": "@bot:matrix.org",
            "password": "secret",
            "device_name": "octos-gw",
        }))
        .unwrap();

        let entry = channel_to_entry(&channel);
        let settings = &entry["settings"];
        assert_eq!(settings["user_id"], "@bot:matrix.org");
        assert_eq!(settings["password"], "secret");
        assert_eq!(settings["device_name"], "octos-gw");
    }

    #[test]
    fn should_roundtrip_tts_provider_and_cloud_on_profile_config() {
        let json =
            r#"{ "tts_provider": "cloud", "tts_cloud": { "appid": "999", "voice": "BV700" } }"#;
        let cfg: ProfileConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.tts_provider.as_deref(), Some("cloud"));
        assert_eq!(
            cfg.tts_cloud.as_ref().unwrap().appid.as_deref(),
            Some("999")
        );
    }

    #[test]
    fn should_default_tts_fields_to_none_when_absent() {
        let cfg: ProfileConfig = serde_json::from_str("{}").unwrap();
        assert!(cfg.tts_provider.is_none());
        assert!(cfg.tts_cloud.is_none());
    }

    // ---- profile config inheritance (global `profile-defaults.json`) ----

    fn inheritance_profile(id: &str) -> UserProfile {
        UserProfile {
            id: id.into(),
            name: id.into(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig::default(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn tool_hook(cmd: &str) -> octos_agent::HookConfig {
        octos_agent::HookConfig {
            event: octos_agent::HookEvent::BeforeToolCall,
            command: vec![cmd.to_string()],
            timeout_ms: 5000,
            tool_filter: Vec::new(),
            path_filter: Vec::new(),
            requires_bin: None,
        }
    }

    fn write_profile_defaults(registry_root: &Path, config: &ProfileConfig) {
        std::fs::write(
            registry_root.join("profile-defaults.json"),
            serde_json::to_string_pretty(config).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn effective_config_inherits_when_profile_leaves_fields_unset() {
        let registry_root = tempfile::tempdir().unwrap();
        let data_root = tempfile::tempdir().unwrap();

        let defaults = ProfileConfig {
            hooks: vec![tool_hook("default-hook")],
            env_vars: HashMap::from([
                ("SHARED".to_string(), "from-default".to_string()),
                ("ONLY_DEFAULT".to_string(), "d".to_string()),
            ]),
            memory: Some(crate::config::MemoryConfig {
                max_inject_tokens: Some(4242),
                refresh: None,
            }),
            approval_policy: Some(crate::config::ApprovalPolicyConfig::default()),
            plugins: crate::config::PluginsConfig {
                require_signed: true,
            },
            sandbox: octos_agent::SandboxConfig {
                allow_network: true,
                ..Default::default()
            },
            // Identity / channel fields on the defaults must NEVER leak into a
            // profile that omits them.
            api_type: Some("anthropic".to_string()),
            admin_mode: true,
            channels: vec![ChannelCredentials::Discord {
                token_env: "DEFAULT_DISCORD".to_string(),
            }],
            ..Default::default()
        };
        write_profile_defaults(registry_root.path(), &defaults);

        let store = ProfileStore::open(registry_root.path(), data_root.path()).unwrap();

        let mut profile = inheritance_profile("alice");
        profile.config.hooks = vec![tool_hook("profile-hook")];
        profile
            .config
            .env_vars
            .insert("SHARED".to_string(), "from-profile".to_string());
        profile
            .config
            .env_vars
            .insert("ONLY_PROFILE".to_string(), "p".to_string());
        profile.config.api_type = Some("openai".to_string());
        // plugins, sandbox, memory, approval_policy left at Default/None.

        let eff = store.effective_config(&profile);

        // hooks: defaults first, then the profile's own (order preserved).
        assert_eq!(eff.hooks.len(), 2);
        assert_eq!(eff.hooks[0].command, vec!["default-hook".to_string()]);
        assert_eq!(eff.hooks[1].command, vec!["profile-hook".to_string()]);

        // env_vars: merged; profile key wins on collision.
        assert_eq!(
            eff.env_vars.get("SHARED").map(String::as_str),
            Some("from-profile")
        );
        assert_eq!(
            eff.env_vars.get("ONLY_DEFAULT").map(String::as_str),
            Some("d")
        );
        assert_eq!(
            eff.env_vars.get("ONLY_PROFILE").map(String::as_str),
            Some("p")
        );

        // memory + approval_policy inherited (profile left them None).
        assert_eq!(eff.memory.as_ref().unwrap().max_inject_tokens, Some(4242));
        assert!(eff.approval_policy.is_some());

        // plugins + sandbox inherited (profile left them at Default).
        assert!(eff.plugins.require_signed);
        assert!(eff.sandbox.allow_network);

        // Identity / channel fields NEVER inherit.
        assert_eq!(eff.api_type.as_deref(), Some("openai"));
        assert!(!eff.admin_mode);
        assert!(
            eff.channels.is_empty(),
            "channels are identity/instance-specific and must not inherit from defaults"
        );
    }

    #[test]
    fn effective_config_prefers_profile_value_over_defaults() {
        let registry_root = tempfile::tempdir().unwrap();
        let data_root = tempfile::tempdir().unwrap();

        let defaults = ProfileConfig {
            memory: Some(crate::config::MemoryConfig {
                max_inject_tokens: Some(1),
                refresh: None,
            }),
            approval_policy: Some(crate::config::ApprovalPolicyConfig::default()),
            // Non-default sandbox: workspace_write=false differs from the
            // profile's own non-default sandbox below.
            sandbox: octos_agent::SandboxConfig {
                workspace_write: false,
                ..Default::default()
            },
            ..Default::default()
        };
        write_profile_defaults(registry_root.path(), &defaults);

        let store = ProfileStore::open(registry_root.path(), data_root.path()).unwrap();

        let mut profile = inheritance_profile("bob");
        profile.config.memory = Some(crate::config::MemoryConfig {
            max_inject_tokens: Some(999),
            refresh: None,
        });
        profile.config.approval_policy = None; // will inherit
        // The profile sets ONE sandbox field (allow_network) and turns signing
        // on, but omits workspace_write.
        profile.config.plugins = crate::config::PluginsConfig {
            require_signed: true,
        };
        profile.config.sandbox = octos_agent::SandboxConfig {
            allow_network: true,
            ..Default::default()
        };

        let eff = store.effective_config(&profile);

        // memory: profile's Some wins over the defaults'.
        assert_eq!(eff.memory.as_ref().unwrap().max_inject_tokens, Some(999));
        // approval_policy: None → inherits the defaults'.
        assert!(eff.approval_policy.is_some());
        // plugins: profile turned signing on; it stays on.
        assert!(eff.plugins.require_signed);
        // sandbox is now a FIELD-BY-FIELD merge, not whole-struct replace:
        // the profile's explicitly-set allow_network stays true, ...
        assert!(eff.sandbox.allow_network);
        // ... while workspace_write, which the profile did NOT set, inherits the
        // defaults' read-only floor (false) instead of reverting to the profile
        // struct's type default (true). This is the FIX-1a correction — before,
        // the whole struct reverted the moment any single field was set.
        assert!(
            !eff.sandbox.workspace_write,
            "an omitted sandbox field must inherit the default restriction even \
             when the profile set a different sandbox field"
        );
    }

    #[test]
    fn effective_config_without_defaults_equals_profile_config() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::open_unified(dir.path()).unwrap();

        let mut profile = inheritance_profile("carol");
        profile.config.hooks = vec![tool_hook("profile-hook")];
        profile
            .config
            .env_vars
            .insert("K".to_string(), "v".to_string());
        profile.config.plugins = crate::config::PluginsConfig {
            require_signed: true,
        };

        // No `profile-defaults.json` ⇒ byte-identical clone (backward compat).
        assert_eq!(store.effective_config(&profile), profile.config);
    }

    #[test]
    fn empty_profile_inherits_default_hooks() {
        let registry_root = tempfile::tempdir().unwrap();
        let data_root = tempfile::tempdir().unwrap();

        let defaults = ProfileConfig {
            hooks: vec![tool_hook("default-hook")],
            ..Default::default()
        };
        write_profile_defaults(registry_root.path(), &defaults);

        let store = ProfileStore::open(registry_root.path(), data_root.path()).unwrap();

        // A freshly-created profile with an empty config already inherits the
        // defaults' hooks at consumption time — no create-time seeding needed.
        let profile = inheritance_profile("frank");
        assert!(profile.config.hooks.is_empty());

        let eff = store.effective_config(&profile);
        assert_eq!(eff.hooks.len(), 1);
        assert_eq!(eff.hooks[0].command, vec!["default-hook".to_string()]);
    }

    #[test]
    fn save_persists_raw_profile_config_not_inherited_defaults() {
        let registry_root = tempfile::tempdir().unwrap();
        let data_root = tempfile::tempdir().unwrap();

        let defaults = ProfileConfig {
            hooks: vec![tool_hook("default-hook")],
            ..Default::default()
        };
        write_profile_defaults(registry_root.path(), &defaults);

        let store = ProfileStore::open(registry_root.path(), data_root.path()).unwrap();

        let mut profile = inheritance_profile("dave");
        profile.config.hooks = vec![tool_hook("profile-hook")];

        // Sanity: the effective view merges both hooks.
        assert_eq!(store.effective_config(&profile).hooks.len(), 2);

        store.save(&profile).unwrap();

        // On-disk JSON must carry ONLY the profile's own hook — the inherited
        // default hook must never be persisted into the profile record.
        let raw = std::fs::read_to_string(store.profile_path("dave")).unwrap();
        assert!(raw.contains("profile-hook"));
        assert!(
            !raw.contains("default-hook"),
            "save() must persist the raw profile config, not the merged effective config"
        );

        // Reload confirms the persisted record still has exactly one hook.
        let reloaded = store.get("dave").unwrap().unwrap();
        assert_eq!(reloaded.config.hooks.len(), 1);
        assert_eq!(
            reloaded.config.hooks[0].command,
            vec!["profile-hook".to_string()]
        );
    }

    #[test]
    fn malformed_defaults_file_opens_store_and_is_ignored() {
        let registry_root = tempfile::tempdir().unwrap();
        let data_root = tempfile::tempdir().unwrap();

        std::fs::write(
            registry_root.path().join("profile-defaults.json"),
            "{ this is not valid json",
        )
        .unwrap();

        // Store still opens despite the malformed defaults file.
        let store = ProfileStore::open(registry_root.path(), data_root.path()).unwrap();

        let mut profile = inheritance_profile("erin");
        profile.config.hooks = vec![tool_hook("profile-hook")];

        // Defaults treated as absent ⇒ effective == profile.config.
        assert_eq!(store.effective_config(&profile), profile.config);
    }

    #[test]
    fn effective_config_field_merges_sandbox_when_profile_sets_one_field() {
        // FIX 1a: a profile that sets ONE sandbox field must still inherit
        // every OTHER default sandbox restriction (not revert the whole struct
        // to type defaults the moment any single field is set).
        let registry_root = tempfile::tempdir().unwrap();
        let data_root = tempfile::tempdir().unwrap();

        let defaults = ProfileConfig {
            sandbox: octos_agent::SandboxConfig {
                workspace_write: false, // read-only workspace floor
                allow_network: true,    // network allowed by default
                ..Default::default()
            },
            ..Default::default()
        };
        write_profile_defaults(registry_root.path(), &defaults);
        let store = ProfileStore::open(registry_root.path(), data_root.path()).unwrap();

        let mut profile = inheritance_profile("gwen");
        // Profile sets ONLY read_allow_paths, leaving every other field unset.
        profile.config.sandbox = octos_agent::SandboxConfig {
            read_allow_paths: vec!["/work".into()],
            ..Default::default()
        };

        let eff = store.effective_config(&profile);

        // The one field the profile set is preserved ...
        assert_eq!(eff.sandbox.read_allow_paths, vec!["/work".to_string()]);
        // ... and the omitted fields inherit the defaults' restrictions.
        assert!(
            !eff.sandbox.workspace_write,
            "omitted workspace_write must inherit the default read-only floor"
        );
        assert!(
            eff.sandbox.allow_network,
            "omitted allow_network must inherit the default"
        );
    }

    // ---- skill layering v1 ----

    fn skill_rule(id: &str, enabled: bool) -> SkillRule {
        SkillRule {
            id: id.to_string(),
            enabled,
        }
    }

    #[test]
    fn merge_skills_none_and_none_is_none() {
        // Byte-identical to pre-skill-layering configs: absent on both sides
        // yields absent, so nothing changes for existing deployments.
        assert_eq!(merge_skills(&None, &None), None);
    }

    #[test]
    fn merge_skills_inherits_defaults_when_profile_absent() {
        let defaults = Some(ProfileSkillsConfig {
            mode: Some(SkillSelectionMode::AllowList),
            rules: vec![skill_rule("news", true)],
        });
        let merged = merge_skills(&defaults, &None).unwrap();
        assert_eq!(merged.mode, Some(SkillSelectionMode::AllowList));
        assert_eq!(merged.rules, vec![skill_rule("news", true)]);
    }

    #[test]
    fn merge_skills_uses_profile_when_defaults_absent() {
        let profile = Some(ProfileSkillsConfig {
            mode: None,
            rules: vec![skill_rule("weather", false)],
        });
        let merged = merge_skills(&None, &profile).unwrap();
        assert_eq!(merged.mode, None);
        assert_eq!(merged.rules, vec![skill_rule("weather", false)]);
    }

    #[test]
    fn merge_skills_replaces_rule_by_id_and_unions() {
        // defaults disable `news` and `weather`; the profile re-enables `news`
        // (replace-by-id) and adds `time` (union). `weather` survives untouched.
        let defaults = Some(ProfileSkillsConfig {
            mode: Some(SkillSelectionMode::AllDiscovered),
            rules: vec![skill_rule("news", false), skill_rule("weather", false)],
        });
        let profile = Some(ProfileSkillsConfig {
            mode: None,
            rules: vec![skill_rule("news", true), skill_rule("time", true)],
        });
        let merged = merge_skills(&defaults, &profile).unwrap();

        // Order: defaults first (news replaced in place, weather kept), then the
        // profile-only `time` appended.
        assert_eq!(
            merged.rules,
            vec![
                skill_rule("news", true), // profile re-enabled (replace-by-id)
                skill_rule("weather", false),
                skill_rule("time", true), // profile-only (union)
            ]
        );
        // A profile may re-enable an inherited disabled rule (ordinary
        // selection, not a security floor).
        assert!(merged.allows("news"));
        assert!(!merged.allows("weather"));
        assert!(merged.allows("time"));
    }

    #[test]
    fn merge_skills_mode_falls_back_to_defaults() {
        // profile.mode None ⇒ inherit defaults' mode.
        let defaults = Some(ProfileSkillsConfig {
            mode: Some(SkillSelectionMode::AllowList),
            rules: vec![],
        });
        let profile = Some(ProfileSkillsConfig {
            mode: None,
            rules: vec![skill_rule("news", true)],
        });
        let merged = merge_skills(&defaults, &profile).unwrap();
        assert_eq!(merged.mode, Some(SkillSelectionMode::AllowList));

        // profile.mode Some ⇒ profile wins.
        let profile_wins = Some(ProfileSkillsConfig {
            mode: Some(SkillSelectionMode::AllDiscovered),
            rules: vec![],
        });
        let merged = merge_skills(&defaults, &profile_wins).unwrap();
        assert_eq!(merged.mode, Some(SkillSelectionMode::AllDiscovered));
    }

    #[test]
    fn effective_config_merges_skills_layer() {
        let registry_root = tempfile::tempdir().unwrap();
        let data_root = tempfile::tempdir().unwrap();

        let defaults = ProfileConfig {
            skills: Some(ProfileSkillsConfig {
                mode: Some(SkillSelectionMode::AllDiscovered),
                rules: vec![skill_rule("news", false)],
            }),
            ..Default::default()
        };
        write_profile_defaults(registry_root.path(), &defaults);
        let store = ProfileStore::open(registry_root.path(), data_root.path()).unwrap();

        let mut profile = inheritance_profile("skilluser");
        profile.config.skills = Some(ProfileSkillsConfig {
            mode: None,
            rules: vec![skill_rule("news", true)], // re-enable inherited disable
        });

        let eff = store.effective_config(&profile);
        let skills = eff
            .skills
            .expect("skills layer must be present after merge");
        // profile.mode None ⇒ inherit defaults' AllDiscovered.
        assert_eq!(skills.mode, Some(SkillSelectionMode::AllDiscovered));
        // profile re-enabled `news`.
        assert!(skills.allows("news"));
    }

    #[test]
    fn effective_config_skills_none_when_neither_sets_it() {
        // No skills anywhere ⇒ None ⇒ every discovered skill loads as before.
        let registry_root = tempfile::tempdir().unwrap();
        let data_root = tempfile::tempdir().unwrap();
        let defaults = ProfileConfig {
            hooks: vec![tool_hook("d")],
            ..Default::default()
        };
        write_profile_defaults(registry_root.path(), &defaults);
        let store = ProfileStore::open(registry_root.path(), data_root.path()).unwrap();

        let profile = inheritance_profile("noskills");
        assert!(store.effective_config(&profile).skills.is_none());
    }

    #[test]
    fn skills_config_roundtrips_and_defaults_to_absent() {
        // Absent field deserializes to None (backwards-compatible).
        let cfg: ProfileConfig = serde_json::from_str("{}").unwrap();
        assert!(cfg.skills.is_none());

        // snake_case mode + rules roundtrip.
        let json = r#"{ "skills": { "mode": "allow_list", "rules": [ { "id": "news", "enabled": true } ] } }"#;
        let cfg: ProfileConfig = serde_json::from_str(json).unwrap();
        let skills = cfg.skills.unwrap();
        assert_eq!(skills.mode, Some(SkillSelectionMode::AllowList));
        assert_eq!(skills.rules, vec![skill_rule("news", true)]);
    }

    #[test]
    fn require_signed_floor_cannot_be_disabled_by_profile() {
        // FIX 1b: `plugins.require_signed` is a one-way ratchet — a defaults
        // `require_signed = true` cannot be turned off by a profile.
        let registry_root = tempfile::tempdir().unwrap();
        let data_root = tempfile::tempdir().unwrap();

        let defaults = ProfileConfig {
            plugins: crate::config::PluginsConfig {
                require_signed: true,
            },
            ..Default::default()
        };
        write_profile_defaults(registry_root.path(), &defaults);
        let store = ProfileStore::open(registry_root.path(), data_root.path()).unwrap();

        let mut profile = inheritance_profile("heidi");
        // Profile explicitly tries to disable signing.
        profile.config.plugins = crate::config::PluginsConfig {
            require_signed: false,
        };

        let eff = store.effective_config(&profile);
        assert!(
            eff.plugins.require_signed,
            "a profile must not be able to disable a defaults-mandated signing floor"
        );
    }

    #[test]
    fn read_allow_paths_floor_clamps_profile_to_operator_roots() {
        // A non-empty defaults `read_allow_paths` is a floor: a profile may
        // only narrow to a subset (paths under an operator root); a path
        // outside every root is dropped.
        let registry_root = tempfile::tempdir().unwrap();
        let data_root = tempfile::tempdir().unwrap();

        let defaults = ProfileConfig {
            sandbox: octos_agent::SandboxConfig {
                read_allow_paths: vec!["/srv/data".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        write_profile_defaults(registry_root.path(), &defaults);
        let store = ProfileStore::open(registry_root.path(), data_root.path()).unwrap();

        // A profile that widens beyond the operator root has the out-of-floor
        // path dropped, keeping only the subpath under `/srv/data`.
        let mut widen = inheritance_profile("ivan");
        widen.config.sandbox = octos_agent::SandboxConfig {
            read_allow_paths: vec!["/srv/data/tenant".into(), "/etc/secret".into()],
            ..Default::default()
        };
        let eff = store.effective_config(&widen);
        assert_eq!(
            eff.sandbox.read_allow_paths,
            vec!["/srv/data/tenant".to_string()],
            "paths outside the operator's read roots must be dropped"
        );

        // A profile whose paths are ALL outside the floor is clamped back to
        // the operator's set (never silently widened to allow-all).
        let mut escape = inheritance_profile("judy");
        escape.config.sandbox = octos_agent::SandboxConfig {
            read_allow_paths: vec!["/etc/secret".into()],
            ..Default::default()
        };
        let eff = store.effective_config(&escape);
        assert_eq!(
            eff.sandbox.read_allow_paths,
            vec!["/srv/data".to_string()],
            "an entirely out-of-floor profile list is clamped to the operator's roots"
        );

        // A profile using `..` to climb out of the operator root must NOT slip
        // through the subset check: `/srv/data/../../etc` resolves to `/etc`,
        // outside the floor, so it is dropped and the list clamps to defaults.
        let mut traverse = inheritance_profile("mallory");
        traverse.config.sandbox = octos_agent::SandboxConfig {
            read_allow_paths: vec!["/srv/data/../../etc".into()],
            ..Default::default()
        };
        let eff = store.effective_config(&traverse);
        assert_eq!(
            eff.sandbox.read_allow_paths,
            vec!["/srv/data".to_string()],
            "a `..` traversal escaping the operator root must be clamped, not honored"
        );

        // Sibling-prefix guard: `/srv/database` must not count as within
        // `/srv/data` (string prefix ≠ path containment).
        let mut sibling = inheritance_profile("neil");
        sibling.config.sandbox = octos_agent::SandboxConfig {
            read_allow_paths: vec!["/srv/database".into()],
            ..Default::default()
        };
        let eff = store.effective_config(&sibling);
        assert_eq!(
            eff.sandbox.read_allow_paths,
            vec!["/srv/data".to_string()],
            "a sibling dir sharing a string prefix must not pass the containment check"
        );
    }

    #[test]
    fn resolve_runtime_profile_applies_parent_and_defaults() {
        // FIX 2: the shared resolver applies BOTH parent/sub-account
        // inheritance AND the global profile-defaults layer.
        let registry_root = tempfile::tempdir().unwrap();
        let data_root = tempfile::tempdir().unwrap();

        let defaults = ProfileConfig {
            hooks: vec![tool_hook("default-hook")],
            env_vars: HashMap::from([("DEFAULT_ONLY".to_string(), "d".to_string())]),
            sandbox: octos_agent::SandboxConfig {
                workspace_write: false,
                ..Default::default()
            },
            ..Default::default()
        };
        write_profile_defaults(registry_root.path(), &defaults);
        let store = ProfileStore::open(registry_root.path(), data_root.path()).unwrap();

        let mut parent = inheritance_profile("acct");
        parent.config.env_vars = HashMap::from([
            ("PARENT_ONLY".to_string(), "p".to_string()),
            ("SHARED".to_string(), "parent".to_string()),
        ]);
        store.save(&parent).unwrap();

        let mut child = inheritance_profile("acct--work");
        child.parent_id = Some("acct".into());
        child.config.hooks = vec![tool_hook("child-hook")];
        child.config.env_vars = HashMap::from([("SHARED".to_string(), "child".to_string())]);
        store.save(&child).unwrap();

        let resolved = store.resolve_runtime_profile(&child);

        // Parent layer: the child inherits the parent-only env var.
        assert_eq!(resolved.config.env_vars.get("PARENT_ONLY").unwrap(), "p");
        // Child wins on a collision with the parent.
        assert_eq!(resolved.config.env_vars.get("SHARED").unwrap(), "child");
        // Defaults layer: the defaults-only env var and hook are present ...
        assert_eq!(resolved.config.env_vars.get("DEFAULT_ONLY").unwrap(), "d");
        assert_eq!(resolved.config.hooks.len(), 2);
        assert_eq!(
            resolved.config.hooks[0].command,
            vec!["default-hook".to_string()]
        );
        assert_eq!(
            resolved.config.hooks[1].command,
            vec!["child-hook".to_string()]
        );
        // Defaults sandbox floor applies through the resolver.
        assert!(!resolved.config.sandbox.workspace_write);
    }

    #[test]
    fn resolve_runtime_profile_without_parent_applies_defaults() {
        // A parentless profile (the serve per-profile-loop shape) still gets
        // the global defaults layer through the shared resolver.
        let registry_root = tempfile::tempdir().unwrap();
        let data_root = tempfile::tempdir().unwrap();

        let defaults = ProfileConfig {
            hooks: vec![tool_hook("default-hook")],
            ..Default::default()
        };
        write_profile_defaults(registry_root.path(), &defaults);
        let store = ProfileStore::open(registry_root.path(), data_root.path()).unwrap();

        let profile = inheritance_profile("kate");
        let resolved = store.resolve_runtime_profile(&profile);
        assert_eq!(resolved.config.hooks.len(), 1);
        assert_eq!(
            resolved.config.hooks[0].command,
            vec!["default-hook".to_string()]
        );
    }

    #[test]
    fn should_roundtrip_canonical_asr_language_on_profile_config() {
        let cfg: ProfileConfig = serde_json::from_str(r#"{ "asr_language": "chinese" }"#).unwrap();
        assert_eq!(cfg.asr_language.as_deref(), Some("Chinese"));
        assert_eq!(
            serde_json::to_value(&cfg).unwrap()["asr_language"],
            "Chinese"
        );
    }

    #[test]
    fn should_inherit_asr_language_when_absent_or_null() {
        let absent: ProfileConfig = serde_json::from_str("{}").unwrap();
        let null: ProfileConfig = serde_json::from_str(r#"{ "asr_language": null }"#).unwrap();
        assert!(absent.asr_language.is_none());
        assert!(null.asr_language.is_none());
    }

    #[test]
    fn should_roundtrip_auto_as_profile_asr_language() {
        let cfg: ProfileConfig = serde_json::from_str(r#"{ "asr_language": "AUTO" }"#).unwrap();
        assert_eq!(cfg.asr_language.as_deref(), Some("auto"));
        assert_eq!(serde_json::to_value(&cfg).unwrap()["asr_language"], "auto");
    }

    #[test]
    fn should_patch_canonical_auto_and_clear_profile_asr_language() {
        let mut config = ProfileConfig::default();
        let patch: ProfileConfigPatch =
            serde_json::from_str(r#"{ "asr_language": "french" }"#).unwrap();
        config.apply_patch(patch);
        assert_eq!(config.asr_language.as_deref(), Some("French"));

        let patch: ProfileConfigPatch =
            serde_json::from_str(r#"{ "asr_language": "AUTO" }"#).unwrap();
        config.apply_patch(patch);
        assert_eq!(config.asr_language.as_deref(), Some("auto"));

        let patch: ProfileConfigPatch =
            serde_json::from_str(r#"{ "asr_language": null }"#).unwrap();
        config.apply_patch(patch);
        assert!(config.asr_language.is_none());
    }

    #[test]
    fn should_prefer_profile_asr_language_over_serve_default() {
        assert_eq!(
            effective_asr_language(Some("English"), Some("zh")),
            Some("English".to_string())
        );
    }

    #[test]
    fn should_preserve_auto_none_and_unknown_inherited_asr_semantics() {
        assert_eq!(
            effective_asr_language(None, Some("zh")),
            Some("Chinese".to_string())
        );
        assert_eq!(
            effective_asr_language(None, Some("en-US")),
            Some("English".to_string())
        );
        assert_eq!(effective_asr_language(None, Some("auto")), None);
        assert_eq!(effective_asr_language(None, None), None);
        assert_eq!(
            effective_asr_language(None, Some("legacy-custom-language")),
            Some("legacy-custom-language".to_string())
        );
        assert_eq!(effective_asr_language(Some("auto"), Some("Chinese")), None);
    }

    #[test]
    fn should_reload_profile_asr_language_from_disk_and_isolate_profiles() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::open_unified(dir.path()).unwrap();
        let now = chrono::Utc::now();
        let mut alpha = UserProfile {
            id: "alpha".into(),
            name: "Alpha".into(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                asr_language: Some("English".into()),
                ..Default::default()
            },
            created_at: now,
            updated_at: now,
        };
        let beta = UserProfile {
            id: "beta".into(),
            name: "Beta".into(),
            config: ProfileConfig {
                asr_language: Some("Japanese".into()),
                ..Default::default()
            },
            ..alpha.clone()
        };
        store.save(&alpha).unwrap();
        store.save(&beta).unwrap();

        assert_eq!(
            effective_profile_asr_language(Some(&store), Some("alpha"), Some("Chinese")).unwrap(),
            Some("English".to_string())
        );
        assert_eq!(
            effective_profile_asr_language(Some(&store), Some("beta"), Some("Chinese")).unwrap(),
            Some("Japanese".to_string())
        );

        alpha.config.asr_language = Some("French".into());
        store.save(&alpha).unwrap();
        assert_eq!(
            effective_profile_asr_language(Some(&store), Some("alpha"), Some("Chinese")).unwrap(),
            Some("French".to_string())
        );
        assert_eq!(
            effective_profile_asr_language(Some(&store), Some("beta"), Some("Chinese")).unwrap(),
            Some("Japanese".to_string())
        );
    }

    #[test]
    fn should_report_missing_and_malformed_profiles_when_resolving_asr_language() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::open_unified(dir.path()).unwrap();

        assert_eq!(
            effective_profile_asr_language(
                Some(&store),
                Some(octos_core::MAIN_PROFILE_ID),
                Some("English"),
            )
            .unwrap(),
            Some("English".to_string())
        );

        let missing =
            effective_profile_asr_language(Some(&store), Some("missing"), None).unwrap_err();
        assert!(missing.to_string().contains("not found"));

        std::fs::write(store.profile_path("broken"), "{not json").unwrap();
        let malformed =
            effective_profile_asr_language(Some(&store), Some("broken"), None).unwrap_err();
        assert!(malformed.to_string().contains("failed to parse profile"));
    }
}
