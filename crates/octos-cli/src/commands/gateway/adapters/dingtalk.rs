use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use eyre::WrapErr;
use octos_bus::ChannelManager;

use super::settings_str;
use crate::config::ChannelEntry;

fn optional_env(name: &str) -> eyre::Result<Option<String>> {
    if name.trim().is_empty() {
        return Ok(None);
    }
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => Ok(Some(value)),
        Ok(_) => Ok(None),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(error).wrap_err_with(|| format!("{name} environment variable invalid")),
    }
}

pub fn register(
    channel_mgr: &mut ChannelManager,
    entry: &ChannelEntry,
    shutdown: &Arc<AtomicBool>,
) -> eyre::Result<()> {
    let webhook_url_env = settings_str(&entry.settings, "webhook_url_env", "DINGTALK_BOT_WEBHOOK");
    let secret_env = settings_str(&entry.settings, "secret_env", "DINGTALK_BOT_SECRET");
    let webhook_url = optional_env(&webhook_url_env)?;
    let secret = optional_env(&secret_env)?;
    let webhook_port: u16 = entry
        .settings
        .get("webhook_port")
        .and_then(|v| v.as_u64())
        .unwrap_or(8650) as u16;

    channel_mgr.register(Arc::new(
        octos_bus::DingTalkChannel::new(
            webhook_url,
            secret,
            entry.allowed_senders.clone(),
            shutdown.clone(),
        )
        .with_webhook_port(webhook_port),
    ));
    Ok(())
}
