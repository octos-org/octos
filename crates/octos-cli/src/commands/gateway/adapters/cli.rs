use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use octos_bus::{ChannelManager, CliChannel};
use tokio::sync::Notify;

use crate::config::ChannelEntry;

pub fn register(
    channel_mgr: &mut ChannelManager,
    _entry: &ChannelEntry,
    shutdown: &Arc<AtomicBool>,
    shutdown_notify: &Arc<Notify>,
) -> eyre::Result<()> {
    channel_mgr.register(Arc::new(CliChannel::with_shutdown_notify(
        shutdown.clone(),
        shutdown_notify.clone(),
    )));
    Ok(())
}
