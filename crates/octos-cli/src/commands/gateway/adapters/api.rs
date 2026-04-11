use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use octos_bus::{ChannelManager, SessionManager};
use octos_core::MAIN_PROFILE_ID;
use tokio::sync::Mutex;

use crate::config::ChannelEntry;

pub fn register(
    channel_mgr: &mut ChannelManager,
    entry: &ChannelEntry,
    shutdown: &Arc<AtomicBool>,
    session_mgr: &Arc<Mutex<SessionManager>>,
    gateway_profile_id: Option<&str>,
    api_port_override: Option<u16>,
) -> eyre::Result<()> {
    let port: u16 = api_port_override.unwrap_or_else(|| {
        entry
            .settings
            .get("port")
            .and_then(|v| v.as_u64())
            .unwrap_or(8091) as u16
    });
    let auth_token = entry
        .settings
        .get("auth_token")
        .and_then(|v| v.as_str())
        .map(String::from);
    let channel = octos_bus::ApiChannel::new(
        port,
        auth_token,
        shutdown.clone(),
        session_mgr.clone(),
        gateway_profile_id.unwrap_or(MAIN_PROFILE_ID).to_string(),
    );
    channel_mgr.register(Arc::new(channel));
    Ok(())
}
