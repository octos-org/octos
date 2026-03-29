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
    let id_env = settings_str(&entry.settings, "app_id_env", "FEISHU_APP_ID");
    let secret_env = settings_str(&entry.settings, "app_secret_env", "FEISHU_APP_SECRET");
    let region = settings_str(&entry.settings, "region", "cn");
    let app_id = std::env::var(&id_env)
        .wrap_err_with(|| format!("{id_env} environment variable not set"))?;
    let app_secret = std::env::var(&secret_env)
        .wrap_err_with(|| format!("{secret_env} environment variable not set"))?;
    let mode = settings_str(&entry.settings, "mode", "ws");
    let webhook_port: u16 = entry
        .settings
        .get("webhook_port")
        .and_then(|v| v.as_u64())
        .unwrap_or(9321) as u16;
    let encrypt_key = entry
        .settings
        .get("encrypt_key")
        .and_then(|v| v.as_str())
        .map(String::from);
    let verification_token = entry
        .settings
        .get("verification_token")
        .and_then(|v| v.as_str())
        .map(String::from);
    channel_mgr.register(Arc::new(
        octos_bus::FeishuChannel::new(
            &app_id,
            &app_secret,
            entry.allowed_senders.clone(),
            shutdown.clone(),
            &region,
            media_dir.to_path_buf(),
        )
        .with_mode(&mode)
        .with_webhook_port(webhook_port)
        .with_encrypt_key(encrypt_key)
        .with_verification_token(verification_token),
    ));
    Ok(())
}
