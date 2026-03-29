//! User profile management for multi-user deployments.
//!
//! Each profile is a named configuration bundle that defines an LLM provider,
//! channel credentials, and gateway settings. Profiles are stored as individual
//! JSON files in `~/.octos/profiles/`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use eyre::{Result, WrapErr, bail};
use serde::{Deserialize, Serialize};

use crate::config::{ChannelEntry, Config, FallbackModel, GatewayConfig};

/// A user profile with all configuration needed to run a gateway.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserProfile {
    /// Unique identifier (slug: lowercase alphanumeric + hyphens).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Whether this profile's gateway should auto-start with the server.
    #[serde(default)]
    pub enabled: bool,
    /// Data directory override. Default: `~/.octos/profiles/{id}/data`
    #[serde(default)]
    pub data_dir: Option<String>,
    /// If set, this profile is a sub-account of the given parent profile.
    /// Sub-accounts inherit LLM provider config (provider, model, base_url,
    /// api_key_env, fallback_models, env_vars) from their parent.
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
    /// LLM provider name (anthropic, openai, moonshot, deepseek, etc.).
    #[serde(default)]
    pub provider: Option<String>,
    /// Model name.
    #[serde(default)]
    pub model: Option<String>,
    /// Custom base URL override. If set, takes priority over provider mapping.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Env var name for API key.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Fallback models for provider failover chain.
    #[serde(default)]
    pub fallback_models: Vec<FallbackModelConfig>,
    /// Channel configurations.
    #[serde(default)]
    pub channels: Vec<ChannelCredentials>,
    /// Gateway-specific settings.
    #[serde(default)]
    pub gateway: GatewaySettings,
    /// Email sending configuration (SMTP or Feishu/Lark).
    #[serde(default)]
    pub email: Option<EmailSettings>,
    /// API protocol type: "openai" or "anthropic". Overrides provider default.
    #[serde(default)]
    pub api_type: Option<String>,
    /// Environment variables to pass to the gateway process (e.g. API keys).
    /// Keys are env var names, values are the actual secrets.
    #[serde(default)]
    pub env_vars: HashMap<String, String>,
    /// Lifecycle hooks for agent events (per-profile).
    #[serde(default)]
    pub hooks: Vec<octos_agent::HookConfig>,
    /// Admin mode: when true, gateway registers only admin management tools
    /// (no shell, file, web, browser tools). Used for the admin bot profile.
    #[serde(default)]
    pub admin_mode: bool,
    /// Sandbox configuration for tool isolation.
    #[serde(default)]
    pub sandbox: octos_agent::SandboxConfig,
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

/// A fallback model entry for the provider failover chain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct FallbackModelConfig {
    /// Provider name (e.g. "openai", "moonshot", "deepseek").
    pub provider: String,
    /// Model name.
    #[serde(default)]
    pub model: Option<String>,
    /// Custom base URL override (for DashScope, MiniMax, NVIDIA NIM, etc.).
    #[serde(default)]
    pub base_url: Option<String>,
    /// Env var name for API key (if different from primary).
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// API protocol type: "openai" or "anthropic".
    #[serde(default)]
    pub api_type: Option<String>,
    /// Published output price in USD per million tokens (for cost-aware routing).
    #[serde(default)]
    pub cost_per_m: Option<f64>,
}

/// Channel-specific credentials (tagged by type).
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
        homeserver: String,
        as_token: String,
        hs_token: String,
        server_name: String,
        #[serde(default = "default_matrix_sender_localpart")]
        sender_localpart: String,
        #[serde(default = "default_matrix_user_prefix")]
        user_prefix: String,
        #[serde(default = "default_matrix_port")]
        port: u16,
        #[serde(default)]
        allowed_senders: Vec<String>,
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
}

fn default_telegram_env() -> String {
    "TELEGRAM_BOT_TOKEN".into()
}
fn default_discord_env() -> String {
    "DISCORD_BOT_TOKEN".into()
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
fn default_qq_bot_secret_env() -> String {
    "QQ_BOT_CLIENT_SECRET".into()
}
fn default_wechat_token_env() -> String {
    "WECHAT_BOT_TOKEN".into()
}
fn default_wechat_base_url() -> String {
    "https://ilinkai.weixin.qq.com".into()
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
}

/// Manages profile storage as individual JSON files.
pub struct ProfileStore {
    profiles_dir: PathBuf,
}

impl ProfileStore {
    /// Open (or create) the profile store at `data_dir/profiles/`.
    pub fn open(data_dir: &Path) -> Result<Self> {
        let profiles_dir = data_dir.join("profiles");
        std::fs::create_dir_all(&profiles_dir).wrap_err_with(|| {
            format!("failed to create profiles dir: {}", profiles_dir.display())
        })?;
        Ok(Self { profiles_dir })
    }

    /// List all profiles sorted by name.
    pub fn list(&self) -> Result<Vec<UserProfile>> {
        let mut profiles = Vec::new();
        let entries = match std::fs::read_dir(&self.profiles_dir) {
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
                        Ok(profile) => profiles.push(profile),
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
        let profile = serde_json::from_str(&content)
            .wrap_err_with(|| format!("failed to parse profile: {id}"))?;
        Ok(Some(profile))
    }

    /// Save a profile (create or update). Also initializes the data directory.
    pub fn save(&self, profile: &UserProfile) -> Result<()> {
        validate_profile_id(&profile.id)?;

        // Initialize data directory structure
        let data_dir = self.resolve_data_dir(profile);
        for sub in ["memory", "sessions", "research", "skills", "history"] {
            std::fs::create_dir_all(data_dir.join(sub)).ok();
        }

        let path = self.profile_path(&profile.id);
        let content =
            serde_json::to_string_pretty(profile).wrap_err("failed to serialize profile")?;

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
                let is_masked = new_val.contains("***")
                    || new_val.contains(KEYCHAIN_DISPLAY)
                    || new_val.is_empty();
                // Never overwrite the real stored value with a display artifact,
                // but DO allow explicit "keychain:" marker (it's the real value).
                if is_masked && new_val != crate::auth::KEYCHAIN_MARKER {
                    if let Some(old_val) = existing.config.env_vars.get(key) {
                        *new_val = old_val.clone();
                    }
                }
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
            self.profiles_dir.join(&profile.id).join("data")
        }
    }

    pub(crate) fn profile_path(&self, id: &str) -> PathBuf {
        self.profiles_dir.join(format!("{id}.json"))
    }

    /// Return the parent directory of the profiles dir (i.e. the octos home dir).
    pub fn octos_home_dir(&self) -> &Path {
        self.profiles_dir.parent().unwrap_or(&self.profiles_dir)
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

    /// Create a sub-account under a parent profile.
    ///
    /// The sub-account inherits LLM provider config from the parent at runtime.
    /// It has its own channels, gateway settings, and data directory.
    pub fn create_sub_account(
        &self,
        parent_id: &str,
        sub_name: &str,
        channels: Vec<ChannelCredentials>,
        gateway: GatewaySettings,
    ) -> Result<UserProfile> {
        // Verify parent exists
        let _parent = self
            .get(parent_id)?
            .ok_or_else(|| eyre::eyre!("parent profile '{parent_id}' not found"))?;

        let sub_id = format!("{parent_id}--{}", slugify(sub_name));
        validate_profile_id(&sub_id)?;

        if self.get(&sub_id)?.is_some() {
            bail!("sub-account '{sub_id}' already exists");
        }

        let now = Utc::now();
        let profile = UserProfile {
            id: sub_id,
            name: sub_name.to_string(),
            enabled: false,
            data_dir: None,
            parent_id: Some(parent_id.to_string()),
            config: ProfileConfig {
                // LLM fields left empty — inherited at runtime from parent
                provider: None,
                model: None,
                base_url: None,
                api_key_env: None,
                fallback_models: vec![],
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

    // Inherit LLM provider config from parent
    ec.provider = pc.provider.clone();
    ec.model = pc.model.clone();
    ec.base_url = pc.base_url.clone();
    ec.api_key_env = pc.api_key_env.clone();
    ec.api_type = pc.api_type.clone();
    ec.fallback_models = pc.fallback_models.clone();

    // Inherit email config if sub-account doesn't have its own
    if ec.email.is_none() {
        ec.email = pc.email.clone();
    }

    // Merge env_vars: parent as base, sub-account overrides win
    let mut merged_env = pc.env_vars.clone();
    merged_env.extend(ec.env_vars.clone());
    ec.env_vars = merged_env;

    Ok(effective)
}

/// Convert a name to a slug (lowercase, non-alphanumeric chars replaced with hyphens).
fn slugify(s: &str) -> String {
    let slug: String = s
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    slug.trim_matches('-').to_string()
}

/// Return a copy of the profile with secret values in `env_vars` masked.
/// Shows the first 4 and last 3 characters for keys longer than 12 chars,
/// otherwise replaces the entire value with `***`.
/// Keychain-backed values show as a special indicator.
pub fn mask_secrets(profile: &UserProfile) -> UserProfile {
    let mut masked = profile.clone();
    for value in masked.config.env_vars.values_mut() {
        if value == crate::auth::KEYCHAIN_MARKER {
            *value = KEYCHAIN_DISPLAY.to_string();
        } else {
            *value = mask_value(value);
        }
    }
    masked
}

/// Display string for keychain-backed values in API responses.
const KEYCHAIN_DISPLAY: &str = "\u{1f511} (keychain)";

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

/// Validate a profile ID (slug format).
fn validate_profile_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 64 {
        bail!("profile ID must be 1-64 characters");
    }
    if !id
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        bail!("profile ID must contain only lowercase letters, digits, and hyphens");
    }
    if id.starts_with('-') || id.ends_with('-') {
        bail!("profile ID must not start or end with a hyphen");
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
            // Override Feishu webhook_port if auto-assigned
            if let ChannelCredentials::Feishu { .. } = ch {
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
        .fallback_models
        .iter()
        .map(|fb| FallbackModel {
            provider: fb.provider.clone(),
            model: fb.model.clone(),
            base_url: fb.base_url.clone(),
            api_key_env: fb.api_key_env.clone(),
            model_hints: None,
            api_type: fb.api_type.clone(),
            cost_per_m: fb.cost_per_m,
        })
        .collect();

    Config {
        provider: profile.config.provider.clone(),
        model: profile.config.model.clone(),
        base_url: profile.config.base_url.clone(),
        api_key_env: profile.config.api_key_env.clone(),
        api_type: profile.config.api_type.clone(),
        max_iterations: profile.config.gateway.max_iterations,
        gateway: Some(GatewayConfig {
            channels,
            max_history: profile.config.gateway.max_history.unwrap_or(50),
            system_prompt: profile.config.gateway.system_prompt.clone(),
            max_concurrent_sessions: profile.config.gateway.max_concurrent_sessions.unwrap_or(10),
            browser_timeout_secs: profile.config.gateway.browser_timeout_secs,
            max_output_tokens: profile.config.gateway.max_output_tokens,
            ..Default::default()
        }),
        fallback_models,
        // Fields not configured through profiles — use defaults
        version: None,
        model_hints: None,
        mcp_servers: vec![],
        sandbox: profile.config.sandbox.clone(),
        tool_policy: None,
        tool_policy_by_provider: Default::default(),
        embedding: None,
        hooks: profile.config.hooks.clone(),
        context_filter: vec![],
        sub_providers: vec![],
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
        adaptive_routing: None,
        voice: None,
        #[cfg(feature = "api")]
        dashboard_auth: None,
        #[cfg(feature = "api")]
        monitor: None,
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
        } => serde_json::json!({
            "type": "matrix",
            "allowed_senders": allowed_senders,
            "settings": {
                "homeserver": homeserver,
                "as_token": as_token,
                "hs_token": hs_token,
                "server_name": server_name,
                "sender_localpart": sender_localpart,
                "user_prefix": user_prefix,
                "port": port,
            }
        }),
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
/// Restart-required: provider, model, base_url, api_key_env, channels,
///   fallback_models, env_vars.
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

    // Provider/model changes are hot-reloadable (switch_model tool does live
    // swap via SwappableProvider and persists to profile; restarting the
    // gateway would kill the in-flight response).
    if oc.provider != nc.provider || oc.model != nc.model {
        tracing::debug!(
            old_provider = ?oc.provider,
            new_provider = ?nc.provider,
            old_model = ?oc.model,
            new_model = ?nc.model,
            "provider/model change detected — treating as hot-reload (switch_model already applied)"
        );
    }
    // base_url and api_key_env still require restart
    if oc.base_url != nc.base_url {
        restart_fields.push("base_url".into());
    }
    if oc.api_key_env != nc.api_key_env {
        restart_fields.push("api_key_env".into());
    }
    if oc.channels != nc.channels {
        restart_fields.push("channels".into());
    }
    if oc.fallback_models != nc.fallback_models {
        restart_fields.push("fallback_models".into());
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
    fn test_validate_profile_id() {
        assert!(validate_profile_id("alice").is_ok());
        assert!(validate_profile_id("team-bot").is_ok());
        assert!(validate_profile_id("user123").is_ok());
        assert!(validate_profile_id("").is_err());
        assert!(validate_profile_id("-bad").is_err());
        assert!(validate_profile_id("bad-").is_err());
        assert!(validate_profile_id("UPPER").is_err());
        assert!(validate_profile_id("has space").is_err());
        assert!(validate_profile_id("a".repeat(65).as_str()).is_err());
    }

    #[test]
    fn test_profile_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::open(dir.path()).unwrap();

        let profile = UserProfile {
            id: "test".into(),
            name: "Test Bot".into(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            config: ProfileConfig {
                provider: Some("anthropic".into()),
                model: Some("claude-sonnet-4-20250514".into()),
                api_key_env: Some("ANTHROPIC_API_KEY".into()),
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
        let loaded = store.get("test").unwrap().unwrap();
        assert_eq!(loaded.id, "test");
        assert_eq!(loaded.name, "Test Bot");
        assert!(loaded.enabled);

        let profiles = store.list().unwrap();
        assert_eq!(profiles.len(), 1);

        assert!(store.delete("test").unwrap());
        assert!(store.get("test").unwrap().is_none());
    }

    #[test]
    fn test_config_from_profile() {
        let profile = UserProfile {
            id: "gen-test".into(),
            name: "Config Gen".into(),
            enabled: false,
            data_dir: None,
            parent_id: None,
            config: ProfileConfig {
                provider: Some("openai".into()),
                model: Some("gpt-4o".into()),
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
            config: ProfileConfig {
                provider: Some("moonshot".into()),
                model: Some("kimi-k2.5".into()),
                api_key_env: Some("MOONSHOT_API_KEY".into()),
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
    fn test_config_from_profile_bridge_url_override() {
        let profile = UserProfile {
            id: "wa-test".into(),
            name: "WA Test".into(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            config: ProfileConfig {
                provider: Some("anthropic".into()),
                model: Some("claude-sonnet-4-20250514".into()),
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
        let store = ProfileStore::open(dir.path()).unwrap();

        let mut profile = UserProfile {
            id: "alice".into(),
            name: "Alice".into(),
            enabled: false,
            data_dir: None,
            parent_id: None,
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
            config: ProfileConfig {
                env_vars: [
                    ("API_KEY".into(), "sk-1234567890abcdef".into()),
                    ("SHORT".into(), "abc".into()),
                ]
                .into(),
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let masked = mask_secrets(&profile);
        assert_eq!(masked.config.env_vars["API_KEY"], "sk-1***def");
        assert_eq!(masked.config.env_vars["SHORT"], "***");
    }

    #[test]
    fn test_file_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::open(dir.path()).unwrap();
        let profile = UserProfile {
            id: "perms-test".into(),
            name: "Perms".into(),
            enabled: false,
            data_dir: None,
            parent_id: None,
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
        let store = ProfileStore::open(dir.path()).unwrap();

        // Save a profile with real secrets
        let original = UserProfile {
            id: "merge-test".into(),
            name: "Merge".into(),
            enabled: false,
            data_dir: None,
            parent_id: None,
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

    #[test]
    fn test_diff_profiles_model_change_is_hot() {
        let base = UserProfile {
            id: "diff-test".into(),
            name: "Diff".into(),
            enabled: false,
            data_dir: None,
            parent_id: None,
            config: ProfileConfig {
                provider: Some("openai".into()),
                model: Some("gpt-4o".into()),
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let mut changed = base.clone();
        changed.config.model = Some("gpt-4o-mini".into());

        // Provider/model changes are hot-reloadable (switch_model does live swap)
        assert!(matches!(
            diff_profiles(&base, &changed),
            ProfileChange::HotReloadable | ProfileChange::Unchanged
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
            config: ProfileConfig {
                provider: Some("openai".into()),
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
    fn test_diff_profiles_unchanged() {
        let base = UserProfile {
            id: "diff-test".into(),
            name: "Diff".into(),
            enabled: false,
            data_dir: None,
            parent_id: None,
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
        let store = ProfileStore::open(dir.path()).unwrap();

        // Create parent with LLM config
        let parent = UserProfile {
            id: "parent".into(),
            name: "Parent Bot".into(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            config: ProfileConfig {
                provider: Some("openai".into()),
                model: Some("gpt-4o".into()),
                api_key_env: Some("OPENAI_API_KEY".into()),
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
        assert!(sub.config.provider.is_none()); // Not set — inherited at runtime
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
                .create_sub_account("parent", "work bot", vec![], GatewaySettings::default())
                .is_err()
        );
    }

    #[test]
    fn test_resolve_effective_profile() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::open(dir.path()).unwrap();

        // Create parent
        let parent = UserProfile {
            id: "parent".into(),
            name: "Parent".into(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            config: ProfileConfig {
                provider: Some("openai".into()),
                model: Some("gpt-4o".into()),
                base_url: Some("https://custom.api.com/v1".into()),
                api_key_env: Some("OPENAI_API_KEY".into()),
                env_vars: [
                    ("OPENAI_API_KEY".into(), "sk-parent-key".into()),
                    ("SHARED_VAR".into(), "parent-value".into()),
                ]
                .into(),
                fallback_models: vec![FallbackModelConfig {
                    provider: "anthropic".into(),
                    model: Some("claude-sonnet-4-20250514".into()),
                    ..Default::default()
                }],
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
        assert_eq!(effective.config.provider.as_deref(), Some("openai"));
        assert_eq!(effective.config.model.as_deref(), Some("gpt-4o"));
        assert_eq!(
            effective.config.base_url.as_deref(),
            Some("https://custom.api.com/v1")
        );
        assert_eq!(effective.config.fallback_models.len(), 1);

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
        assert_eq!(effective_parent.config.provider.as_deref(), Some("openai"));
    }

    #[test]
    fn test_diff_profiles_parent_id_change() {
        let base = UserProfile {
            id: "sub".into(),
            name: "Sub".into(),
            enabled: false,
            data_dir: None,
            parent_id: Some("parent-a".into()),
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
            other => panic!("expected RestartRequired, got {:?}", other),
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
        assert_eq!(parsed.len(), 6);
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
        let store = ProfileStore::open(dir.path()).unwrap();

        // Save profile with keychain marker
        let original = UserProfile {
            id: "kc-merge".into(),
            name: "KC Merge".into(),
            enabled: false,
            data_dir: None,
            parent_id: None,
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
        let store = ProfileStore::open(dir.path()).unwrap();

        // Profile with plaintext secret
        let original = UserProfile {
            id: "kc-set".into(),
            name: "KC Set".into(),
            enabled: false,
            data_dir: None,
            parent_id: None,
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
        let store = ProfileStore::open(dir.path()).unwrap();

        let original = UserProfile {
            id: "kc-empty".into(),
            name: "KC Empty".into(),
            enabled: false,
            data_dir: None,
            parent_id: None,
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
}
