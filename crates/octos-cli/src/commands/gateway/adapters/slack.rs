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
    let bot_env = settings_str(&entry.settings, "bot_token_env", "SLACK_BOT_TOKEN");
    let app_env = settings_str(&entry.settings, "app_token_env", "SLACK_APP_TOKEN");
    let bot_token = std::env::var(&bot_env)
        .wrap_err_with(|| format!("{bot_env} environment variable not set"))?;
    let app_token = std::env::var(&app_env)
        .wrap_err_with(|| format!("{app_env} environment variable not set"))?;
    channel_mgr.register(Arc::new(octos_bus::SlackChannel::new(
        &bot_token,
        &app_token,
        entry.allowed_senders.clone(),
        shutdown.clone(),
        media_dir.to_path_buf(),
    )));
    Ok(())
}
