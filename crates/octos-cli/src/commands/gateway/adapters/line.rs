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
    let secret_env = settings_str(&entry.settings, "channel_secret_env", "LINE_CHANNEL_SECRET");
    let token_env = settings_str(
        &entry.settings,
        "channel_access_token_env",
        "LINE_CHANNEL_ACCESS_TOKEN",
    );
    let channel_secret = std::env::var(&secret_env)
        .wrap_err_with(|| format!("{secret_env} environment variable not set"))?;
    let channel_access_token = std::env::var(&token_env)
        .wrap_err_with(|| format!("{token_env} environment variable not set"))?;
    let webhook_port: u16 = entry
        .settings
        .get("webhook_port")
        .and_then(|v| v.as_u64())
        .unwrap_or(8646) as u16;
    let bot_user_id = entry
        .settings
        .get("bot_user_id")
        .and_then(|v| v.as_str())
        .map(String::from);
    let require_mention = entry
        .settings
        .get("require_mention")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut line = octos_bus::LineChannel::new(
        &channel_secret,
        &channel_access_token,
        entry.allowed_senders.clone(),
        shutdown.clone(),
        media_dir.to_path_buf(),
    )
    .with_webhook_port(webhook_port);
    if require_mention {
        line = line.with_mention_gating(bot_user_id);
    }

    channel_mgr.register(Arc::new(line));
    Ok(())
}
