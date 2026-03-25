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
    let app_id = settings_str(&entry.settings, "app_id", "");
    let client_secret_env =
        settings_str(&entry.settings, "client_secret_env", "QQ_BOT_CLIENT_SECRET");
    let client_secret = std::env::var(&client_secret_env)
        .wrap_err_with(|| format!("{client_secret_env} environment variable not set"))?;
    if app_id.is_empty() {
        eyre::bail!("qq-bot channel requires settings.app_id");
    }
    channel_mgr.register(Arc::new(octos_bus::QQBotChannel::new(
        &app_id,
        &client_secret,
        entry.allowed_senders.clone(),
        shutdown.clone(),
    )));
    Ok(())
}
