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
    let env = settings_str(&entry.settings, "token_env", "TELEGRAM_BOT_TOKEN");
    let token =
        std::env::var(&env).wrap_err_with(|| format!("{env} environment variable not set"))?;
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
    channel_mgr.register(Arc::new(tg));
    Ok(())
}
