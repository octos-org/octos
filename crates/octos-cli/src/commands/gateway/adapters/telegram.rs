use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use eyre::WrapErr;
use octos_bus::ChannelManager;

use super::{BotOwners, settings_str};
use crate::bot_owner::BotOwner;
use crate::config::ChannelEntry;

pub fn register(
    channel_mgr: &mut ChannelManager,
    entry: &ChannelEntry,
    shutdown: &Arc<AtomicBool>,
    media_dir: &Path,
    owners: &mut BotOwners<'_>,
) -> eyre::Result<()> {
    let env = settings_str(&entry.settings, "token_env", "TELEGRAM_BOT_TOKEN");
    let token =
        std::env::var(&env).wrap_err_with(|| format!("{env} environment variable not set"))?;
    // T-0654: the bot is this gateway's alone, or the gateway does not start.
    if let Some(owner) = BotOwner::claim_for_gateway(&token, owners.dir, owners.holder)
        .map_err(|why| eyre::eyre!(why))?
    {
        owners.held.push(owner);
    }
    let bot_username = entry
        .settings
        .get("bot_username")
        .and_then(|v| v.as_str())
        .map(String::from);
    let require_mention = entry
        .settings
        .get("require_mention")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let mut tg = octos_bus::TelegramChannel::new(
        &token,
        entry.allowed_senders.clone(),
        shutdown.clone(),
        media_dir.to_path_buf(),
    );
    if require_mention {
        tg = tg.with_mention_gating(bot_username);
    }
    // #1506 — explicit chat → profile map. A malformed map is a hard config
    // error (channel refused at startup), never a silent fall-back to the
    // default profile.
    let profile_map = octos_bus::profile_map::ChatProfileMap::from_settings(&entry.settings)
        .map_err(|error| eyre::eyre!("telegram chat_profile_map config error: {error}"))?;
    tg = tg.with_profile_map(profile_map);
    channel_mgr.register(Arc::new(tg));
    Ok(())
}
