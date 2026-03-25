use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use octos_bus::ChannelManager;

use super::super::matrix_integration::*;
use crate::config::ChannelEntry;

pub fn register(
    channel_mgr: &mut ChannelManager,
    matrix_channel: &mut Option<Arc<octos_bus::MatrixChannel>>,
    entry: &ChannelEntry,
    shutdown: &Arc<AtomicBool>,
    data_dir: &Path,
) -> eyre::Result<()> {
    let settings = MatrixChannelSettings::from_entry(entry)?;
    let _ = register_matrix_channel(channel_mgr, matrix_channel, &settings, shutdown, data_dir);
    Ok(())
}
