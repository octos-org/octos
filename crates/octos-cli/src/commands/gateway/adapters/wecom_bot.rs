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
) -> eyre::Result<()> {
    let bot_id = settings_str(&entry.settings, "bot_id", "");
    let secret_env = settings_str(&entry.settings, "secret_env", "WECOM_BOT_SECRET");
    let secret = std::env::var(&secret_env)
        .wrap_err_with(|| format!("{secret_env} environment variable not set"))?;
    if bot_id.is_empty() {
        eyre::bail!("wecom-bot channel requires settings.bot_id");
    }
    channel_mgr.register(Arc::new(octos_bus::WeComBotChannel::new(
        &bot_id,
        &secret,
        entry.allowed_senders.clone(),
        shutdown.clone(),
    )));
    Ok(())
}
