//! Message bus, channels, and session management for octos gateway.

pub mod bus;
pub mod channel;
pub mod cli_channel;
pub mod coalesce;
pub mod cron_service;
pub mod cron_types;
pub mod dedup;
pub mod file_handle;
pub mod heartbeat;
pub mod markdown_html;
pub mod media;
pub mod session;

#[cfg(feature = "api")]
pub mod api_channel;
#[cfg(feature = "discord")]
pub mod discord_channel;
#[cfg(feature = "email")]
pub mod email_channel;
#[cfg(feature = "feishu")]
pub mod feishu_channel;
#[cfg(feature = "matrix")]
pub mod matrix_channel;
#[cfg(feature = "qq-bot")]
pub mod qq_bot_channel;
#[cfg(feature = "slack")]
pub mod slack_channel;
#[cfg(feature = "telegram")]
pub mod telegram_channel;
#[cfg(feature = "twilio")]
pub mod twilio_channel;
#[cfg(feature = "wechat")]
pub mod wechat_channel;
#[cfg(feature = "wecom-bot")]
pub mod wecom_bot_channel;
#[cfg(feature = "wecom")]
pub mod wecom_channel;
#[cfg(feature = "wecom")]
pub(crate) mod wecom_crypto;
#[cfg(feature = "whatsapp")]
pub mod whatsapp_channel;

pub use bus::{AgentHandle, BusPublisher, create_bus};
pub use channel::{Channel, ChannelHealth, ChannelManager};
pub use cli_channel::CliChannel;
pub use cron_service::CronService;
pub use cron_types::{CronJob, CronPayload, CronSchedule, CronStore};
pub use dedup::MessageDedup;
pub use heartbeat::HeartbeatService;
pub use session::{
    ActiveSessionStore, Session, SessionHandle, SessionListEntry, SessionManager,
    validate_topic_name,
};

#[cfg(feature = "api")]
pub use api_channel::ApiChannel;
#[cfg(feature = "discord")]
pub use discord_channel::DiscordChannel;
#[cfg(feature = "email")]
pub use email_channel::EmailChannel;
#[cfg(feature = "feishu")]
pub use feishu_channel::FeishuChannel;
#[cfg(feature = "matrix")]
pub use matrix_channel::{BotEntry, BotManager, BotRouter, BotVisibility, MatrixChannel};
#[cfg(feature = "qq-bot")]
pub use qq_bot_channel::QQBotChannel;
#[cfg(feature = "slack")]
pub use slack_channel::SlackChannel;
#[cfg(feature = "telegram")]
pub use telegram_channel::TelegramChannel;
#[cfg(feature = "twilio")]
pub use twilio_channel::TwilioChannel;
#[cfg(feature = "wechat")]
pub use wechat_channel::WeChatChannel;
#[cfg(feature = "wecom-bot")]
pub use wecom_bot_channel::WeComBotChannel;
#[cfg(feature = "wecom")]
pub use wecom_channel::WeComChannel;
#[cfg(feature = "whatsapp")]
pub use whatsapp_channel::WhatsAppChannel;
