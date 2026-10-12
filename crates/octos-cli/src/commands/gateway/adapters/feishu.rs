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
    let encrypt_key = entry.settings.get("encrypt_key").and_then(|v| v.as_str());
    // An empty encrypt_key must not arm the channel: sha256("") is publicly
    // known, so signature verification and event decryption would both accept
    // attacker-crafted material (#2772). Treat it as unconfigured like the
    // verification_token below; tell the operator their setting was dropped.
    if encrypt_key.is_some_and(|key| key.is_empty()) {
        tracing::warn!(
            "Feishu channel: encrypt_key is set but empty — treated as unconfigured \
             (webhook signature verification will stay DISABLED)"
        );
    }
    let encrypt_key = encrypt_key.filter(|key| !key.is_empty()).map(String::from);
    // An empty-string placeholder must not arm the channel into refusing
    // every event (#2756 review): treat it as unconfigured.
    let verification_token = entry
        .settings
        .get("verification_token")
        .and_then(|v| v.as_str())
        .filter(|token| !token.is_empty())
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
