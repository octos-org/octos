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
    let corp_id_env = settings_str(&entry.settings, "corp_id_env", "WECOM_CORP_ID");
    let secret_env = settings_str(&entry.settings, "agent_secret_env", "WECOM_AGENT_SECRET");
    let corp_id = std::env::var(&corp_id_env)
        .wrap_err_with(|| format!("{corp_id_env} environment variable not set"))?;
    let agent_secret = std::env::var(&secret_env)
        .wrap_err_with(|| format!("{secret_env} environment variable not set"))?;
    let agent_id = settings_str(&entry.settings, "agent_id", "");
    let verification_token = settings_str(&entry.settings, "verification_token", "");
    let encoding_aes_key = settings_str(&entry.settings, "encoding_aes_key", "");
    let webhook_port: u16 = entry
        .settings
        .get("webhook_port")
        .and_then(|v| v.as_u64())
        .unwrap_or(9322) as u16;
    channel_mgr.register(Arc::new(
        octos_bus::WeComChannel::new(
            &corp_id,
            &agent_id,
            &agent_secret,
            &verification_token,
            &encoding_aes_key,
            entry.allowed_senders.clone(),
            shutdown.clone(),
            media_dir.to_path_buf(),
        )
        .with_webhook_port(webhook_port),
    ));
    Ok(())
}
