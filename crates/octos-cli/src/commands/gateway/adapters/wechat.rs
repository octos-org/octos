use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use octos_bus::ChannelManager;

use super::settings_str;
use crate::config::ChannelEntry;

pub fn register(
    channel_mgr: &mut ChannelManager,
    entry: &ChannelEntry,
    shutdown: &Arc<AtomicBool>,
    wechat_bridge_url: Option<&str>,
) -> eyre::Result<()> {
    let default_url = settings_str(&entry.settings, "bridge_url", "ws://localhost:3201");
    let bridge_url = wechat_bridge_url.unwrap_or(&default_url);
    channel_mgr.register(Arc::new(octos_bus::WeChatChannel::new(
        bridge_url,
        entry.allowed_senders.clone(),
        shutdown.clone(),
    )));
    Ok(())
}
