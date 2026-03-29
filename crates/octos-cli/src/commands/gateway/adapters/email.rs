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
    let imap_host = settings_str(&entry.settings, "imap_host", "");
    let imap_port = entry
        .settings
        .get("imap_port")
        .and_then(|v| v.as_u64())
        .unwrap_or(993) as u16;
    let smtp_host = settings_str(&entry.settings, "smtp_host", "");
    let smtp_port = entry
        .settings
        .get("smtp_port")
        .and_then(|v| v.as_u64())
        .unwrap_or(465) as u16;
    let user_env = settings_str(&entry.settings, "username_env", "EMAIL_USERNAME");
    let pass_env = settings_str(&entry.settings, "password_env", "EMAIL_PASSWORD");
    let username = std::env::var(&user_env).wrap_err_with(|| format!("{user_env} not set"))?;
    let password = std::env::var(&pass_env).wrap_err_with(|| format!("{pass_env} not set"))?;
    let from_address = settings_str(&entry.settings, "from_address", &username);
    let poll_interval = entry
        .settings
        .get("poll_interval_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(30);
    let max_body_chars = entry
        .settings
        .get("max_body_chars")
        .and_then(|v| v.as_u64())
        .unwrap_or(10000) as usize;

    let email_config = octos_bus::email_channel::EmailConfig {
        imap_host,
        imap_port,
        smtp_host,
        smtp_port,
        username,
        password,
        from_address,
        poll_interval_secs: poll_interval,
        allowed_senders: entry.allowed_senders.clone(),
        max_body_chars,
    };
    channel_mgr.register(Arc::new(octos_bus::EmailChannel::new(
        email_config,
        shutdown.clone(),
    )));
    Ok(())
}
