use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use octos_bus::{ChannelManager, CliChannel};

use crate::config::ChannelEntry;

pub fn register(
    channel_mgr: &mut ChannelManager,
    _entry: &ChannelEntry,
    shutdown: &Arc<AtomicBool>,
) -> eyre::Result<()> {
    channel_mgr.register(Arc::new(CliChannel::new(shutdown.clone())));
    Ok(())
}
