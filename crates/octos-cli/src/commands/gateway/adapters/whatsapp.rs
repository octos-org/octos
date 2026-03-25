use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use octos_bus::ChannelManager;

use super::settings_str;
use crate::config::ChannelEntry;

pub fn register(
    channel_mgr: &mut ChannelManager,
    entry: &ChannelEntry,
    shutdown: &Arc<AtomicBool>,
    media_dir: &Path,
) -> eyre::Result<()> {
    let url = settings_str(&entry.settings, "bridge_url", "ws://localhost:3001");
    channel_mgr.register(Arc::new(octos_bus::WhatsAppChannel::new(
        &url,
        entry.allowed_senders.clone(),
        shutdown.clone(),
        media_dir.to_path_buf(),
    )));
    Ok(())
}
