use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use octos_bus::ChannelManager;
use tracing::warn;

#[cfg(feature = "matrix")]
use crate::cron_tool::CronTool;

use super::prompt::settings_str;

#[cfg(all(feature = "matrix", test))]
pub(super) const MATRIX_CHANNEL_TYPE: &str = "matrix";
#[cfg(feature = "matrix")]
pub(super) const MATRIX_SETTING_HOMESERVER: &str = "homeserver";
#[cfg(feature = "matrix")]
pub(super) const MATRIX_SETTING_AS_TOKEN: &str = "as_token";
#[cfg(feature = "matrix")]
pub(super) const MATRIX_SETTING_HS_TOKEN: &str = "hs_token";
#[cfg(feature = "matrix")]
pub(super) const MATRIX_SETTING_SERVER_NAME: &str = "server_name";
#[cfg(feature = "matrix")]
pub(super) const MATRIX_SETTING_SENDER_LOCALPART: &str = "sender_localpart";
#[cfg(feature = "matrix")]
pub(super) const MATRIX_SETTING_USER_PREFIX: &str = "user_prefix";
#[cfg(feature = "matrix")]
pub(super) const MATRIX_DEFAULT_HOMESERVER: &str = "http://localhost:6167";
#[cfg(feature = "matrix")]
pub(super) const MATRIX_DEFAULT_SERVER_NAME: &str = "localhost";
#[cfg(feature = "matrix")]
pub(super) const MATRIX_DEFAULT_SENDER_LOCALPART: &str = "bot";
#[cfg(feature = "matrix")]
pub(super) const MATRIX_DEFAULT_USER_PREFIX: &str = "bot_";
#[cfg(feature = "matrix")]
pub(super) const MATRIX_DEFAULT_PORT: u16 = 8009;
#[cfg(feature = "matrix")]
pub(super) const MATRIX_MISSING_TOKENS_ERROR: &str =
    "matrix channel requires settings.as_token and settings.hs_token";
#[cfg(feature = "matrix")]
pub(super) const MATRIX_BOT_USER_ID_ENV_KEY: &str = "OCTOS_MATRIX_BOT_USER_ID";

// ── User-mode (Matrix account login) settings ────────────────────────────────
#[cfg(feature = "matrix")]
pub(super) const MATRIX_SETTING_MODE: &str = "mode";
#[cfg(feature = "matrix")]
pub(super) const MATRIX_MODE_USER: &str = "user";
#[cfg(feature = "matrix")]
pub(super) const MATRIX_MODE_APPSERVICE: &str = "appservice";
#[cfg(feature = "matrix")]
pub(super) const MATRIX_SETTING_USER_ID: &str = "user_id";
#[cfg(feature = "matrix")]
pub(super) const MATRIX_SETTING_ACCESS_TOKEN: &str = "access_token";
#[cfg(feature = "matrix")]
pub(super) const MATRIX_SETTING_PASSWORD: &str = "password";
#[cfg(feature = "matrix")]
pub(super) const MATRIX_SETTING_DEVICE_NAME: &str = "device_name";
#[cfg(feature = "matrix")]
pub(super) const MATRIX_SETTING_ROOMS: &str = "rooms";
#[cfg(feature = "matrix")]
pub(super) const MATRIX_SETTING_AUTO_JOIN: &str = "auto_join";
#[cfg(feature = "matrix")]
pub(super) const MATRIX_SETTING_AUTO_JOIN_CAMEL: &str = "autoJoin";
#[cfg(feature = "matrix")]
pub(super) const MATRIX_SETTING_AUTO_JOIN_ALLOWLIST: &str = "auto_join_allowlist";
#[cfg(feature = "matrix")]
pub(super) const MATRIX_SETTING_AUTO_JOIN_ALLOWLIST_CAMEL: &str = "autoJoinAllowlist";
#[cfg(feature = "matrix")]
pub(super) const MATRIX_SETTING_GROUP_POLICY: &str = "group_policy";
#[cfg(feature = "matrix")]
pub(super) const MATRIX_SETTING_GROUP_POLICY_CAMEL: &str = "groupPolicy";
#[cfg(feature = "matrix")]
pub(super) const MATRIX_SETTING_REQUIRE_MENTION: &str = "require_mention";
#[cfg(feature = "matrix")]
pub(super) const MATRIX_SETTING_REQUIRE_MENTION_CAMEL: &str = "requireMention";
#[cfg(feature = "matrix")]
pub(super) const MATRIX_USER_MISSING_AUTH_ERROR: &str =
    "matrix user channel requires settings.access_token or settings.user_id + settings.password";

/// Returns `true` when the channel entry selects user-account (client) mode.
/// Defaults to appservice mode when `mode` is unset.
#[cfg(feature = "matrix")]
pub(super) fn matrix_is_user_mode(entry: &crate::config::ChannelEntry) -> bool {
    settings_str(&entry.settings, MATRIX_SETTING_MODE, MATRIX_MODE_APPSERVICE)
        .eq_ignore_ascii_case(MATRIX_MODE_USER)
}

#[cfg(feature = "matrix")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct MatrixChannelSettings {
    pub(super) homeserver: String,
    pub(super) as_token: String,
    pub(super) hs_token: String,
    pub(super) server_name: String,
    pub(super) sender_localpart: String,
    pub(super) user_prefix: String,
    pub(super) port: u16,
    pub(super) allowed_senders: Vec<String>,
}

#[cfg(feature = "matrix")]
impl MatrixChannelSettings {
    pub(super) fn from_entry(entry: &crate::config::ChannelEntry) -> Result<Self> {
        let homeserver = settings_str(
            &entry.settings,
            MATRIX_SETTING_HOMESERVER,
            MATRIX_DEFAULT_HOMESERVER,
        );
        let as_token = settings_str(&entry.settings, MATRIX_SETTING_AS_TOKEN, "");
        let hs_token = settings_str(&entry.settings, MATRIX_SETTING_HS_TOKEN, "");
        if as_token.is_empty() || hs_token.is_empty() {
            eyre::bail!(MATRIX_MISSING_TOKENS_ERROR);
        }

        let port = match entry.settings.get("port").and_then(|v| v.as_u64()) {
            Some(raw) => u16::try_from(raw)
                .map_err(|_| eyre::eyre!("matrix channel port out of range: {raw}"))?,
            None => MATRIX_DEFAULT_PORT,
        };

        Ok(Self {
            homeserver,
            as_token,
            hs_token,
            server_name: settings_str(
                &entry.settings,
                MATRIX_SETTING_SERVER_NAME,
                MATRIX_DEFAULT_SERVER_NAME,
            ),
            sender_localpart: settings_str(
                &entry.settings,
                MATRIX_SETTING_SENDER_LOCALPART,
                MATRIX_DEFAULT_SENDER_LOCALPART,
            ),
            user_prefix: settings_str(
                &entry.settings,
                MATRIX_SETTING_USER_PREFIX,
                MATRIX_DEFAULT_USER_PREFIX,
            ),
            port,
            allowed_senders: entry.allowed_senders.clone(),
        })
    }

    fn build_channel(
        &self,
        shutdown: Arc<AtomicBool>,
        data_dir: &std::path::Path,
    ) -> Arc<octos_bus::MatrixChannel> {
        Arc::new(
            octos_bus::MatrixChannel::new(
                &self.homeserver,
                &self.as_token,
                &self.hs_token,
                &self.server_name,
                &self.sender_localpart,
                &self.user_prefix,
                self.port,
                shutdown,
            )
            .with_admin_allowed_senders(self.allowed_senders.clone())
            .with_media_dir(data_dir.join("media"))
            .with_bot_router(data_dir),
        )
    }
}

#[cfg(feature = "matrix")]
fn get_or_create_matrix_channel(
    matrix_channel: &mut Option<Arc<octos_bus::MatrixChannel>>,
    settings: &MatrixChannelSettings,
    shutdown: &Arc<AtomicBool>,
    data_dir: &std::path::Path,
) -> Arc<octos_bus::MatrixChannel> {
    if let Some(channel) = matrix_channel.clone() {
        channel
    } else {
        let channel = settings.build_channel(shutdown.clone(), data_dir);
        *matrix_channel = Some(channel.clone());
        channel
    }
}

#[cfg(feature = "matrix")]
pub(super) fn register_matrix_channel(
    channel_mgr: &mut ChannelManager,
    matrix_channel: &mut Option<Arc<octos_bus::MatrixChannel>>,
    settings: &MatrixChannelSettings,
    shutdown: &Arc<AtomicBool>,
    data_dir: &std::path::Path,
) -> Arc<octos_bus::MatrixChannel> {
    let channel = get_or_create_matrix_channel(matrix_channel, settings, shutdown, data_dir);
    channel_mgr.register(channel.clone());
    channel
}

/// Settings for the user-account (client) Matrix channel.
///
/// Logs in as a regular Matrix account via an access token or password and
/// long-polls `/sync` — no homeserver-side appservice registration needed.
#[cfg(feature = "matrix")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct MatrixUserChannelSettings {
    pub(super) homeserver: String,
    pub(super) user_id: Option<String>,
    pub(super) access_token: Option<String>,
    pub(super) password: Option<String>,
    pub(super) device_name: Option<String>,
    pub(super) rooms: Vec<String>,
    pub(super) auto_join: octos_bus::MatrixAutoJoin,
    pub(super) auto_join_allowlist: Vec<String>,
    pub(super) group_policy: octos_bus::MatrixGroupPolicy,
    pub(super) require_mention: bool,
    pub(super) allowed_senders: Vec<String>,
}

#[cfg(feature = "matrix")]
impl MatrixUserChannelSettings {
    pub(super) fn from_entry(entry: &crate::config::ChannelEntry) -> Result<Self> {
        let opt = |key: &str| {
            let value = settings_str(&entry.settings, key, "");
            if value.trim().is_empty() {
                None
            } else {
                Some(value)
            }
        };

        let opt_any = |keys: &[&str]| {
            keys.iter()
                .map(|key| settings_str(&entry.settings, key, ""))
                .find(|value| !value.trim().is_empty())
        };

        let access_token = opt(MATRIX_SETTING_ACCESS_TOKEN);
        let user_id = opt(MATRIX_SETTING_USER_ID);
        let password = opt(MATRIX_SETTING_PASSWORD);
        if access_token.is_none() && (user_id.is_none() || password.is_none()) {
            eyre::bail!(MATRIX_USER_MISSING_AUTH_ERROR);
        }

        let string_array = |keys: &[&str]| {
            keys.iter()
                .find_map(|key| entry.settings.get(key))
                .and_then(|value| {
                    if let Some(arr) = value.as_array() {
                        Some(
                            arr.iter()
                                .filter_map(|v| v.as_str())
                                .map(str::to_string)
                                .collect::<Vec<_>>(),
                        )
                    } else {
                        value.as_str().map(|raw| {
                            raw.split(',')
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .map(str::to_string)
                                .collect::<Vec<_>>()
                        })
                    }
                })
                .unwrap_or_default()
        };

        let auto_join = opt_any(&[MATRIX_SETTING_AUTO_JOIN, MATRIX_SETTING_AUTO_JOIN_CAMEL])
            .map(|raw| octos_bus::MatrixAutoJoin::parse(&raw))
            .unwrap_or_default();
        let group_policy = opt_any(&[
            MATRIX_SETTING_GROUP_POLICY,
            MATRIX_SETTING_GROUP_POLICY_CAMEL,
        ])
        .map(|raw| octos_bus::MatrixGroupPolicy::parse(&raw))
        .unwrap_or_default();
        let require_mention = entry
            .settings
            .get(MATRIX_SETTING_REQUIRE_MENTION)
            .or_else(|| entry.settings.get(MATRIX_SETTING_REQUIRE_MENTION_CAMEL))
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        Ok(Self {
            homeserver: settings_str(
                &entry.settings,
                MATRIX_SETTING_HOMESERVER,
                MATRIX_DEFAULT_HOMESERVER,
            ),
            user_id,
            access_token,
            password,
            device_name: opt(MATRIX_SETTING_DEVICE_NAME),
            rooms: string_array(&[MATRIX_SETTING_ROOMS]),
            auto_join,
            auto_join_allowlist: string_array(&[
                MATRIX_SETTING_AUTO_JOIN_ALLOWLIST,
                MATRIX_SETTING_AUTO_JOIN_ALLOWLIST_CAMEL,
            ]),
            group_policy,
            require_mention,
            allowed_senders: entry.allowed_senders.clone(),
        })
    }

    fn build_channel(
        &self,
        shutdown: Arc<AtomicBool>,
        data_dir: &std::path::Path,
        channel_index: usize,
    ) -> Arc<octos_bus::MatrixUserChannel> {
        Arc::new(
            octos_bus::MatrixUserChannel::new(
                &self.homeserver,
                self.user_id.clone(),
                self.access_token.clone(),
                self.password.clone(),
                self.device_name.clone(),
                self.rooms.clone(),
                self.auto_join,
                self.auto_join_allowlist.clone(),
                self.group_policy,
                self.require_mention,
                self.allowed_senders.clone(),
                shutdown,
            )
            .with_channel_index(channel_index)
            .with_invite_store(octos_bus::MatrixInviteStore::for_profile_data_dir(data_dir)),
        )
    }
}

#[cfg(feature = "matrix")]
pub(super) fn register_matrix_user_channel(
    channel_mgr: &mut ChannelManager,
    settings: &MatrixUserChannelSettings,
    shutdown: &Arc<AtomicBool>,
    data_dir: &std::path::Path,
    channel_index: usize,
) -> Arc<octos_bus::MatrixUserChannel> {
    let channel = settings.build_channel(shutdown.clone(), data_dir, channel_index);
    channel_mgr.register(channel.clone());
    channel
}

/// Bot lifecycle manager for slash commands in Matrix rooms.
///
/// Operates inside the running gateway (async context), uses `MatrixChannel`
/// directly for virtual user registration (no HTTP round-trip to self).
#[cfg(feature = "matrix")]
pub(super) struct GatewayBotManager {
    pub(super) store: Arc<crate::profiles::ProfileStore>,
    pub(super) channel: Arc<octos_bus::MatrixChannel>,
    pub(super) parent_profile_id: String,
    pub(super) cron_service: Arc<octos_bus::CronService>,
}

#[cfg(feature = "matrix")]
#[async_trait]
impl octos_bus::BotManager for GatewayBotManager {
    async fn create_bot(
        &self,
        username: &str,
        name: &str,
        system_prompt: Option<&str>,
        sender: &str,
        visibility: octos_bus::BotVisibility,
    ) -> eyre::Result<String> {
        use crate::profiles::GatewaySettings;

        if username.is_empty()
            || !username
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            eyre::bail!(
                "username `{username}` is invalid — only lowercase letters, digits, and underscores are allowed"
            );
        }

        let parent = self
            .store
            .get(&self.parent_profile_id)?
            .ok_or_else(|| eyre::eyre!("parent profile '{}' not found", self.parent_profile_id))?;

        let (server_name, user_prefix) = parent
            .config
            .channels
            .iter()
            .find_map(|ch| {
                if let crate::profiles::ChannelCredentials::Matrix {
                    server_name,
                    user_prefix,
                    ..
                } = ch
                {
                    Some((server_name.clone(), user_prefix.clone()))
                } else {
                    None
                }
            })
            .ok_or_else(|| eyre::eyre!("parent profile has no Matrix channel"))?;

        let matrix_user_id = format!("@{user_prefix}{username}:{server_name}");

        let gateway = GatewaySettings {
            system_prompt: system_prompt.map(str::to_string),
            ..Default::default()
        };
        let mut sub = self.store.create_sub_account(
            &self.parent_profile_id,
            username,
            username,
            name,
            vec![],
            gateway,
        )?;
        sub.name = name.to_string();
        sub.config.env_vars.insert(
            MATRIX_BOT_USER_ID_ENV_KEY.to_string(),
            matrix_user_id.clone(),
        );
        sub.updated_at = chrono::Utc::now();
        self.store.save(&sub)?;

        if let Err(e) = self
            .channel
            .register_bot_owned(&matrix_user_id, &sub.id, sender, visibility)
            .await
        {
            if let Err(delete_error) = self.store.delete(&sub.id) {
                warn!(
                    profile_id = %sub.id,
                    error = %delete_error,
                    "failed to roll back Matrix bot profile after registration failure"
                );
            }
            eyre::bail!("Failed to register Matrix user: {e}");
        }

        let visibility_label = match visibility {
            octos_bus::BotVisibility::Public => "public",
            octos_bus::BotVisibility::Private => "private",
        };

        Ok(format!(
            "Bot **{name}** created successfully!\n\
             \n\
             - Matrix ID: `{matrix_user_id}`\n\
             - Profile ID: `{}`\n\
             - Visibility: `{visibility_label}`\n\
             \n\
             You can now send a direct message to `{matrix_user_id}` to start chatting.",
            sub.id
        ))
    }

    async fn delete_bot(&self, matrix_user_id: &str, sender: &str) -> eyre::Result<String> {
        let botfather_user_id = self.channel.bot_user_id();
        if matrix_user_id == botfather_user_id {
            eyre::bail!("the BotFather bot (`{botfather_user_id}`) cannot be deleted");
        }

        let entry = self
            .channel
            .bot_router()
            .get_entry(matrix_user_id)
            .await
            .ok_or_else(|| {
                eyre::eyre!(
                    "bot `{matrix_user_id}` not found — use `/listbots` to see registered bots"
                )
            })?;
        if !self.channel.is_operator_sender(sender) {
            if entry.owner.is_empty() {
                eyre::bail!(
                    "bot `{matrix_user_id}` is a legacy bot and can only be deleted by an operator"
                );
            }
            if entry.owner != sender {
                eyre::bail!("You can only delete bots you created.");
            }
        }
        let profile_id = entry.profile_id;

        if profile_id == self.parent_profile_id {
            eyre::bail!("the parent profile cannot be deleted");
        }

        let profile = self
            .store
            .get(&profile_id)?
            .ok_or_else(|| eyre::eyre!("profile `{profile_id}` no longer exists"))?;

        if !self.store.delete(&profile_id)? {
            eyre::bail!("profile `{profile_id}` no longer exists");
        }

        if let Err(error) = self.channel.unregister_bot(matrix_user_id).await {
            self.store.save(&profile).wrap_err_with(|| {
                format!(
                    "failed to unregister bot `{matrix_user_id}` and failed to restore profile `{profile_id}`"
                )
            })?;
            return Err(error.wrap_err(format!(
                "failed to unregister bot `{matrix_user_id}`; profile `{profile_id}` restored"
            )));
        }

        Ok(format!(
            "Bot `{matrix_user_id}` deleted (profile `{profile_id}` removed)."
        ))
    }

    async fn list_bots(&self, sender: &str) -> eyre::Result<String> {
        let mut public_lines = Vec::new();
        let mut private_lines = Vec::new();

        let mut entries = self.channel.bot_router().list_entries().await;
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        for (matrix_id, entry) in entries {
            let display_name = self
                .store
                .get(&entry.profile_id)
                .ok()
                .flatten()
                .map(|p| {
                    if p.name.is_empty() {
                        format!("`{matrix_id}`")
                    } else {
                        format!("**{}** `{matrix_id}`", p.name)
                    }
                })
                .unwrap_or_else(|| format!("`{matrix_id}`"));

            match entry.visibility {
                octos_bus::BotVisibility::Public => {
                    let suffix = if entry.owner == sender {
                        " (yours)"
                    } else {
                        ""
                    };
                    if entry.owner.is_empty() && self.channel.is_operator_sender(sender) {
                        public_lines.push(format!("• {display_name} [legacy-ownerless]{suffix}"));
                    } else {
                        public_lines.push(format!("• {display_name}{suffix}"));
                    }
                }
                octos_bus::BotVisibility::Private => {
                    if entry.owner == sender {
                        private_lines.push(format!("• {display_name}"));
                    }
                }
            }
        }

        let mut output = Vec::new();
        if !public_lines.is_empty() {
            output.push("**Public bots:**".to_string());
            output.extend(public_lines);
        }
        if !private_lines.is_empty() {
            if !output.is_empty() {
                output.push(String::new());
            }
            output.push("**Your private bots:**".to_string());
            output.extend(private_lines);
        }
        if output.is_empty() {
            output.push("No bots available.".to_string());
        }

        Ok(output.join("\n"))
    }

    async fn schedule_bot_task(
        &self,
        request: &str,
        _sender: &str,
        room_id: &str,
    ) -> eyre::Result<String> {
        Ok(CronTool::add_natural_language_for_context(
            &self.cron_service,
            "matrix",
            room_id,
            request,
        )?
        .output)
    }

    async fn list_schedules(&self, _sender: &str, room_id: &str) -> eyre::Result<String> {
        Ok(CronTool::list_jobs_for_context(self.cron_service.as_ref(), "matrix", room_id).output)
    }

    async fn unschedule_bot_task(
        &self,
        job_id: &str,
        _sender: &str,
        room_id: &str,
    ) -> eyre::Result<String> {
        Ok(CronTool::remove_job_for_context(&self.cron_service, "matrix", room_id, job_id).output)
    }
}
