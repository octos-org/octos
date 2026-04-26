use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use octos_bus::{ChannelManager, SessionManager};
use tokio::sync::Mutex;

use super::{TaskCancelCb, TaskRelaunchCb};
use crate::config::ChannelEntry;

#[allow(clippy::too_many_arguments)]
pub fn register(
    channel_mgr: &mut ChannelManager,
    entry: &ChannelEntry,
    shutdown: &Arc<AtomicBool>,
    session_mgr: &Arc<Mutex<SessionManager>>,
    metrics_handle: Option<metrics_exporter_prometheus::PrometheusHandle>,
    task_query: Option<Arc<dyn Fn(&str) -> serde_json::Value + Send + Sync>>,
    task_cancel: Option<TaskCancelCb>,
    task_relaunch: Option<TaskRelaunchCb>,
    gateway_profile_id: Option<&str>,
    api_port_override: Option<u16>,
    on_session_deleted: Option<Arc<dyn Fn(&str) + Send + Sync>>,
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
    let mut channel = octos_bus::ApiChannel::new(
        port,
        auth_token,
        shutdown.clone(),
        session_mgr.clone(),
        gateway_profile_id.map(str::to_string),
    );
    if let Some(handle) = metrics_handle {
        channel = channel.with_metrics_renderer(Arc::new(move || handle.render()));
    }
    if let Some(task_query) = task_query {
        channel = channel.with_task_query(task_query);
    }
    if let Some(cancel) = task_cancel {
        channel = channel.with_task_cancel(cancel);
    }
    if let Some(relaunch) = task_relaunch {
        channel = channel.with_task_relaunch(relaunch);
    }
    if let Some(cb) = on_session_deleted {
        channel = channel.with_on_session_deleted(move |id| cb(id));
    }
    channel_mgr.register(Arc::new(channel));
    Ok(())
}
