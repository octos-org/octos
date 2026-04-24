use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use octos_bus::{ChannelManager, SessionManager};
use tokio::sync::Mutex;

use crate::commands::gateway::adapters::TaskLifecycleCallbacks;
use crate::config::ChannelEntry;

pub fn register(
    channel_mgr: &mut ChannelManager,
    entry: &ChannelEntry,
    shutdown: &Arc<AtomicBool>,
    session_mgr: &Arc<Mutex<SessionManager>>,
    metrics_handle: Option<metrics_exporter_prometheus::PrometheusHandle>,
    task_query: Option<Arc<dyn Fn(&str) -> serde_json::Value + Send + Sync>>,
    task_lifecycle: Option<TaskLifecycleCallbacks>,
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
    if let Some(dispatcher) = task_lifecycle {
        let cancel_dispatcher = dispatcher.clone();
        let relaunch_dispatcher = dispatcher.clone();
        let send_dispatcher = dispatcher.clone();
        let cancel_fn: Arc<octos_bus::TaskCancelFn> = Arc::new(
            move |_session_key: &str, task_id: &str, reason: &str| {
                cancel_dispatcher.cancel(task_id, reason)
            },
        );
        let relaunch_fn: Arc<octos_bus::TaskRelaunchFn> = Arc::new(
            move |_session_key: &str, task_id: &str, overrides: serde_json::Value| {
                relaunch_dispatcher.relaunch(task_id, overrides)
            },
        );
        let send_fn: Arc<octos_bus::TaskSendFn> = Arc::new(
            move |_session_key: &str, task_id: &str, message: &str, sender: Option<&str>| {
                send_dispatcher.send(task_id, message, sender)
            },
        );
        channel = channel.with_task_lifecycle(cancel_fn, relaunch_fn, send_fn);
    }
    if let Some(cb) = on_session_deleted {
        channel = channel.with_on_session_deleted(move |id| cb(id));
    }
    channel_mgr.register(Arc::new(channel));
    Ok(())
}
