//! Per-channel adapter registration.
//!
//! Each submodule corresponds to one messaging channel type and exposes a
//! `register()` function that reads the channel entry settings and registers
//! the concrete channel with the [`ChannelManager`].

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use octos_bus::{ChannelManager, SessionManager};
use tokio::sync::Mutex;

use crate::config::ChannelEntry;

#[cfg(feature = "api")]
mod api;
mod cli;
#[cfg(feature = "dingtalk")]
mod dingtalk;
#[cfg(feature = "discord")]
mod discord;
#[cfg(feature = "email")]
mod email;
#[cfg(feature = "feishu")]
mod feishu;
#[cfg(feature = "line")]
mod line;
#[cfg(feature = "matrix")]
mod matrix;
#[cfg(feature = "qq-bot")]
mod qq_bot;
#[cfg(feature = "slack")]
mod slack;
#[cfg(feature = "telegram")]
mod telegram;
#[cfg(feature = "twilio")]
mod twilio;
#[cfg(feature = "wechat")]
mod wechat;
#[cfg(feature = "wecom")]
mod wecom;
#[cfg(feature = "wecom-bot")]
mod wecom_bot;
#[cfg(feature = "whatsapp")]
mod whatsapp;

/// Re-export `settings_str` so individual adapter files can `use super::settings_str`.
#[allow(unused_imports)]
#[cfg(any(
    feature = "telegram",
    feature = "discord",
    feature = "dingtalk",
    feature = "slack",
    feature = "whatsapp",
    feature = "email",
    feature = "feishu",
    feature = "twilio",
    feature = "wecom",
    feature = "wecom-bot",
    feature = "line",
    feature = "matrix",
    feature = "qq-bot",
    feature = "wechat"
))]
pub(crate) use super::prompt::settings_str;

pub type TaskQueryFn = Arc<dyn Fn(&str) -> serde_json::Value + Send + Sync>;
pub type SessionDeletedCallback = Arc<dyn Fn(&str) + Send + Sync>;
/// M7.9 / W2: cancel callback signature shared with the api adapter.
#[cfg(feature = "api")]
pub type TaskCancelCb = Arc<dyn Fn(&str) -> octos_bus::TaskCancelOutcome + Send + Sync>;
/// M7.9 / W2: relaunch callback signature shared with the api adapter.
#[cfg(feature = "api")]
pub type TaskRelaunchCb =
    Arc<dyn Fn(&str, Option<&str>) -> octos_bus::TaskRelaunchOutcome + Send + Sync>;

/// Context needed by adapters that require extra parameters beyond the common set.
#[allow(dead_code)]
pub struct ChannelRegistrationCtx<'a> {
    pub shutdown: &'a Arc<AtomicBool>,
    pub media_dir: &'a Path,
    pub data_dir: &'a Path,
    pub session_mgr: &'a Arc<Mutex<SessionManager>>,
    #[cfg(feature = "api")]
    pub metrics_handle: Option<metrics_exporter_prometheus::PrometheusHandle>,
    #[cfg(not(feature = "api"))]
    pub metrics_handle: Option<()>,
    pub task_query: Option<TaskQueryFn>,
    /// M7.9 / W2: optional cancel callback for the api adapter.
    #[cfg(feature = "api")]
    pub task_cancel: Option<TaskCancelCb>,
    /// M7.9 / W2: optional relaunch callback for the api adapter.
    #[cfg(feature = "api")]
    pub task_relaunch: Option<TaskRelaunchCb>,
    pub gateway_profile_id: Option<&'a str>,
    pub api_port_override: Option<u16>,
    pub wechat_bridge_url: Option<&'a str>,
    /// Callback to stop the session actor when a session is deleted via API.
    pub on_session_deleted: Option<SessionDeletedCallback>,
    #[cfg(feature = "matrix")]
    pub matrix_channel: &'a mut Option<Arc<octos_bus::MatrixChannel>>,
}

/// Register all configured channels with the channel manager.
pub fn register_all(
    channel_mgr: &mut ChannelManager,
    entries: &[ChannelEntry],
    ctx: &mut ChannelRegistrationCtx<'_>,
) -> eyre::Result<()> {
    #[cfg(feature = "matrix")]
    ensure_single_matrix_channel(entries)?;

    for (channel_index, entry) in entries.iter().enumerate() {
        // `channel_index` is only consumed by the matrix arm below.
        #[cfg(not(feature = "matrix"))]
        let _ = channel_index;
        match entry.channel_type.as_str() {
            "cli" => cli::register(channel_mgr, entry, ctx.shutdown)?,
            #[cfg(feature = "telegram")]
            "telegram" => telegram::register(channel_mgr, entry, ctx.shutdown, ctx.media_dir)?,
            #[cfg(feature = "discord")]
            "discord" => discord::register(channel_mgr, entry, ctx.shutdown, ctx.media_dir)?,
            #[cfg(feature = "dingtalk")]
            "dingtalk" => dingtalk::register(channel_mgr, entry, ctx.shutdown)?,
            #[cfg(feature = "slack")]
            "slack" => slack::register(channel_mgr, entry, ctx.shutdown, ctx.media_dir)?,
            #[cfg(feature = "whatsapp")]
            "whatsapp" => whatsapp::register(channel_mgr, entry, ctx.shutdown, ctx.media_dir)?,
            #[cfg(feature = "email")]
            "email" => email::register(channel_mgr, entry, ctx.shutdown)?,
            #[cfg(feature = "feishu")]
            "feishu" | "lark" => feishu::register(channel_mgr, entry, ctx.shutdown, ctx.media_dir)?,
            #[cfg(feature = "twilio")]
            "twilio" => twilio::register(channel_mgr, entry, ctx.shutdown, ctx.media_dir)?,
            #[cfg(feature = "wecom")]
            "wecom" => wecom::register(channel_mgr, entry, ctx.shutdown, ctx.media_dir)?,
            #[cfg(feature = "api")]
            "api" => api::register(
                channel_mgr,
                entry,
                ctx.shutdown,
                ctx.session_mgr,
                ctx.metrics_handle.clone(),
                ctx.task_query.clone(),
                ctx.task_cancel.clone(),
                ctx.task_relaunch.clone(),
                ctx.gateway_profile_id,
                ctx.api_port_override,
                ctx.on_session_deleted.clone(),
            )?,
            #[cfg(feature = "wecom-bot")]
            "wecom-bot" => wecom_bot::register(channel_mgr, entry, ctx.shutdown)?,
            #[cfg(feature = "line")]
            "line" => line::register(channel_mgr, entry, ctx.shutdown, ctx.media_dir)?,
            #[cfg(feature = "matrix")]
            "matrix" => matrix::register(
                channel_mgr,
                ctx.matrix_channel,
                entry,
                channel_index,
                ctx.shutdown,
                ctx.data_dir,
            )?,
            #[cfg(feature = "qq-bot")]
            "qq-bot" => qq_bot::register(channel_mgr, entry, ctx.shutdown)?,
            #[cfg(feature = "wechat")]
            "wechat" => wechat::register(channel_mgr, entry, ctx.shutdown, ctx.wechat_bridge_url)?,
            other => {
                tracing::warn!(channel = other, "channel not supported, skipping");
            }
        }
    }
    Ok(())
}

#[cfg(feature = "matrix")]
fn ensure_single_matrix_channel(entries: &[ChannelEntry]) -> eyre::Result<()> {
    let mut first_index: Option<usize> = None;
    for (idx, entry) in entries.iter().enumerate() {
        if entry.channel_type != "matrix" {
            continue;
        }
        if let Some(first) = first_index {
            eyre::bail!(
                "multiple Matrix channels are not supported yet; channel indexes {first} and {idx} share the same routing key"
            );
        }
        first_index = Some(idx);
    }
    Ok(())
}

#[cfg(all(test, feature = "matrix"))]
mod tests {
    use super::*;

    fn entry(channel_type: &str) -> ChannelEntry {
        ChannelEntry {
            channel_type: channel_type.to_string(),
            allowed_senders: Vec::new(),
            settings: serde_json::json!({}),
        }
    }

    #[test]
    fn rejects_multiple_matrix_channels_before_registration() {
        let entries = vec![entry("cli"), entry("matrix"), entry("matrix")];

        let err = ensure_single_matrix_channel(&entries).unwrap_err();

        assert!(err.to_string().contains("multiple Matrix channels"));
        assert!(err.to_string().contains("1 and 2"));
    }

    #[test]
    fn allows_single_matrix_channel() {
        let entries = vec![entry("cli"), entry("matrix")];

        ensure_single_matrix_channel(&entries).unwrap();
    }
}
