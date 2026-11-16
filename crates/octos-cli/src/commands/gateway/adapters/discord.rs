use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use eyre::WrapErr;
use octos_bus::ChannelManager;

use super::settings_str;
use crate::config::ChannelEntry;

pub fn register(
    channel_mgr: &mut ChannelManager,
    entry: &ChannelEntry,
    shutdown: &Arc<AtomicBool>,
    media_dir: &Path,
) -> eyre::Result<()> {
    let env = settings_str(&entry.settings, "token_env", "DISCORD_BOT_TOKEN");
    let token =
        std::env::var(&env).wrap_err_with(|| format!("{env} environment variable not set"))?;
    // #1506 — explicit channel/guild → profile map. A malformed map is a
    // hard config error (channel refused at startup), never a silent
    // fall-back to the default profile.
    let profile_map = octos_bus::profile_map::ChatProfileMap::from_settings(&entry.settings)
        .map_err(|error| eyre::eyre!("discord chat_profile_map config error: {error}"))?;
    channel_mgr.register(Arc::new(
        octos_bus::DiscordChannel::new(
            &token,
            entry.allowed_senders.clone(),
            shutdown.clone(),
            media_dir.to_path_buf(),
        )
        .with_profile_map(profile_map),
    ));
    Ok(())
}
