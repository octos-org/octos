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
    channel_index: usize,
    shutdown: &Arc<AtomicBool>,
    data_dir: &Path,
) -> eyre::Result<()> {
    // User-account (client) mode: log in with a Matrix account and long-poll
    // `/sync`. Leaves `matrix_channel` (the appservice handle) unset — there is
    // no virtual-user/bot management in this mode.
    if matrix_is_user_mode(entry) {
        let settings = MatrixUserChannelSettings::from_entry(entry)?;
        let _ =
            register_matrix_user_channel(channel_mgr, &settings, shutdown, data_dir, channel_index);
        return Ok(());
    }

    let settings = MatrixChannelSettings::from_entry(entry)?;
    let _ = register_matrix_channel(channel_mgr, matrix_channel, &settings, shutdown, data_dir);
    Ok(())
}
