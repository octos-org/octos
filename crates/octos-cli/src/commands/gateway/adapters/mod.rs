//! Per-channel adapter registration.
//!
//! Each submodule corresponds to one messaging channel type and exposes a
//! `register()` function that reads the channel entry settings and registers
//! the concrete channel with the [`ChannelManager`].

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use octos_bus::{ChannelManager, SessionManager};
use tokio::sync::{Mutex, Notify};

#[cfg(feature = "matrix")]
use super::matrix_integration::matrix_is_user_mode;
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
    pub shutdown_notify: &'a Arc<Notify>,
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
    pub matrix_channels: &'a mut std::collections::HashMap<String, Arc<octos_bus::MatrixChannel>>,
}

/// Register all configured channels with the channel manager.
pub fn register_all(
    channel_mgr: &mut ChannelManager,
    entries: &[ChannelEntry],
    ctx: &mut ChannelRegistrationCtx<'_>,
) -> eyre::Result<()> {
    validate_channel_instances(entries)?;

    for (channel_index, entry) in entries.iter().enumerate() {
        // `channel_index` is only consumed by the matrix arm below.
        #[cfg(not(feature = "matrix"))]
        let _ = channel_index;
        match entry.channel_type.as_str() {
            "cli" => cli::register(channel_mgr, entry, ctx.shutdown, ctx.shutdown_notify)?,
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
                ctx.matrix_channels,
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

fn validate_channel_instances(entries: &[ChannelEntry]) -> eyre::Result<()> {
    let mut routes = std::collections::HashMap::<String, usize>::new();
    #[cfg(feature = "matrix")]
    let mut appservice_ports = std::collections::HashMap::<u64, usize>::new();
    for (idx, entry) in entries.iter().enumerate() {
        if let Some(id) = entry.id.as_deref() {
            let valid = !id.is_empty()
                && id.len() <= 64
                && !id.starts_with('-')
                && !id.ends_with('-')
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
            if !valid {
                eyre::bail!(
                    "channel index {idx} ({}) has invalid id `{id}`; use 1-64 lowercase letters, digits, or hyphens",
                    entry.channel_type
                );
            }
        }

        let route = entry.routing_key();
        if let Some(first) = routes.insert(route.clone(), idx) {
            eyre::bail!(
                "channel indexes {first} and {idx} share routing key `{route}`; give each additional channel of the same type a unique `id`"
            );
        }

        #[cfg(feature = "matrix")]
        if entry.channel_type == "matrix" && !matrix_is_user_mode(entry) {
            let port = entry
                .settings
                .get("port")
                .and_then(|value| value.as_u64())
                .unwrap_or(super::matrix_integration::MATRIX_DEFAULT_PORT as u64);
            if let Some(first) = appservice_ports.insert(port, idx) {
                eyre::bail!(
                    "Matrix appservice channel indexes {first} and {idx} both listen on port {port}; configure a unique port for each appservice"
                );
            }
        }
    }
    Ok(())
}

#[cfg(all(test, feature = "matrix"))]
mod tests {
    use super::*;

    fn entry(channel_type: &str) -> ChannelEntry {
        ChannelEntry {
            channel_type: channel_type.to_string(),
            id: None,
            allowed_senders: Vec::new(),
            settings: serde_json::json!({}),
        }
    }

    #[test]
    fn rejects_multiple_legacy_matrix_routes() {
        let entries = vec![entry("cli"), entry("matrix"), entry("matrix")];

        let err = validate_channel_instances(&entries).unwrap_err();

        assert!(err.to_string().contains("share routing key `matrix`"));
        assert!(err.to_string().contains("indexes 1 and 2"));
    }

    #[test]
    fn allows_legacy_and_named_matrix_channels() {
        let mut named = entry("matrix");
        named.id = Some("work".into());
        named.settings = serde_json::json!({"mode": "user"});
        let entries = vec![entry("cli"), entry("matrix")];

        validate_channel_instances(&entries).unwrap();

        let entries = vec![entry("matrix"), named];
        validate_channel_instances(&entries).unwrap();
    }

    #[test]
    fn rejects_duplicate_matrix_instance_ids() {
        let mut first = entry("matrix");
        first.id = Some("work".into());
        let mut second = entry("matrix");
        second.id = Some("work".into());

        let err = validate_channel_instances(&[first, second]).unwrap_err();
        assert!(err.to_string().contains("matrix@work"));
    }

    #[test]
    fn rejects_invalid_matrix_instance_id() {
        let mut invalid = entry("matrix");
        invalid.id = Some("Work account".into());

        let err = validate_channel_instances(&[invalid]).unwrap_err();
        assert!(err.to_string().contains("invalid id"));
    }

    #[test]
    fn rejects_duplicate_matrix_appservice_ports() {
        let mut first = entry("matrix");
        first.id = Some("primary".into());
        let mut second = entry("matrix");
        second.id = Some("secondary".into());

        let err = validate_channel_instances(&[first, second]).unwrap_err();
        assert!(err.to_string().contains("both listen on port 8009"));
    }

    #[test]
    fn allows_multiple_named_non_matrix_channels() {
        let mut support = entry("telegram");
        support.id = Some("support".into());
        let mut operations = entry("telegram");
        operations.id = Some("operations".into());

        validate_channel_instances(&[entry("telegram"), support, operations]).unwrap();
    }

    #[test]
    fn rejects_duplicate_legacy_non_matrix_routes() {
        let err = validate_channel_instances(&[entry("discord"), entry("discord")]).unwrap_err();
        assert!(err.to_string().contains("routing key `discord`"));
    }
}
