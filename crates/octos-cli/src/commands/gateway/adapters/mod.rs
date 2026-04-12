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
#[cfg(feature = "discord")]
mod discord;
#[cfg(feature = "email")]
mod email;
#[cfg(feature = "feishu")]
mod feishu;
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
    feature = "slack",
    feature = "whatsapp",
    feature = "email",
    feature = "feishu",
    feature = "twilio",
    feature = "wecom",
    feature = "wecom-bot",
    feature = "matrix",
    feature = "qq-bot",
    feature = "wechat"
))]
pub(crate) use super::prompt::settings_str;

/// Context needed by adapters that require extra parameters beyond the common set.
#[allow(dead_code)]
pub struct ChannelRegistrationCtx<'a> {
    pub shutdown: &'a Arc<AtomicBool>,
    pub media_dir: &'a Path,
    pub data_dir: &'a Path,
    pub session_mgr: &'a Arc<Mutex<SessionManager>>,
    pub gateway_profile_id: Option<&'a str>,
    pub api_port_override: Option<u16>,
    pub wechat_bridge_url: Option<&'a str>,
    #[cfg(feature = "matrix")]
    pub matrix_channel: &'a mut Option<Arc<octos_bus::MatrixChannel>>,
}

/// Register all configured channels with the channel manager.
pub fn register_all(
    channel_mgr: &mut ChannelManager,
    entries: &[ChannelEntry],
    ctx: &mut ChannelRegistrationCtx<'_>,
) -> eyre::Result<()> {
    for entry in entries {
        match entry.channel_type.as_str() {
            "cli" => cli::register(channel_mgr, entry, ctx.shutdown)?,
            #[cfg(feature = "telegram")]
            "telegram" => telegram::register(channel_mgr, entry, ctx.shutdown, ctx.media_dir)?,
            #[cfg(feature = "discord")]
            "discord" => discord::register(channel_mgr, entry, ctx.shutdown, ctx.media_dir)?,
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
                ctx.gateway_profile_id,
                ctx.api_port_override,
            )?,
            #[cfg(feature = "wecom-bot")]
            "wecom-bot" => wecom_bot::register(channel_mgr, entry, ctx.shutdown)?,
            #[cfg(feature = "matrix")]
            "matrix" => matrix::register(
                channel_mgr,
                ctx.matrix_channel,
                entry,
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
