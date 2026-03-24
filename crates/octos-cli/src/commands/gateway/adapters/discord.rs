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
    channel_mgr.register(Arc::new(octos_bus::DiscordChannel::new(
        &token,
        entry.allowed_senders.clone(),
        shutdown.clone(),
        media_dir.to_path_buf(),
    )));
    Ok(())
}
