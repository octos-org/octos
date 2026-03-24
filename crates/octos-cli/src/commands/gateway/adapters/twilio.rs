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
    let sid_env = settings_str(&entry.settings, "account_sid_env", "TWILIO_ACCOUNT_SID");
    let token_env = settings_str(&entry.settings, "auth_token_env", "TWILIO_AUTH_TOKEN");
    let from_number = settings_str(&entry.settings, "from_number", "");
    let account_sid = std::env::var(&sid_env)
        .wrap_err_with(|| format!("{sid_env} environment variable not set"))?;
    let auth_token = std::env::var(&token_env)
        .wrap_err_with(|| format!("{token_env} environment variable not set"))?;
    let webhook_port: u16 = entry
        .settings
        .get("webhook_port")
        .and_then(|v| v.as_u64())
        .unwrap_or(8090) as u16;
    channel_mgr.register(Arc::new(octos_bus::TwilioChannel::new(
        &account_sid,
        &auth_token,
        &from_number,
        entry.allowed_senders.clone(),
        shutdown.clone(),
        media_dir.to_path_buf(),
        webhook_port,
    )));
    Ok(())
}
