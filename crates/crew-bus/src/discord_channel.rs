//! Discord channel using serenity gateway + REST API.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use chrono::Utc;
use crew_core::{InboundMessage, OutboundMessage};
use eyre::{Result, WrapErr};
use reqwest::Client as HttpClient;
use serenity::Client;
use serenity::all::{
    Context, EditMessage, EventHandler, GatewayIntents, Http, Message as DiscordMessage, MessageId,
    Ready,
};
use serenity::builder::{CreateAttachment, CreateMessage};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::channel::Channel;
use crate::media::download_media;

pub struct DiscordChannel {
    token: String,
    http: Arc<Http>,
    allowed_senders: HashSet<String>,
    shutdown: Arc<AtomicBool>,
    media_dir: PathBuf,
}

impl DiscordChannel {
    pub fn new(
        token: &str,
        allowed_senders: Vec<String>,
        shutdown: Arc<AtomicBool>,
        media_dir: PathBuf,
    ) -> Self {
        let http = Arc::new(Http::new(token));
        Self {
            token: token.to_string(),
            http,
            allowed_senders: allowed_senders.into_iter().collect(),
            shutdown,
            media_dir,
        }
    }
}

/// Internal handler that forwards Discord messages to the inbound bus.
struct Handler {
    inbound_tx: mpsc::Sender<InboundMessage>,
    allowed_senders: HashSet<String>,
    media_dir: PathBuf,
    download_http: HttpClient,
}

#[async_trait]
impl EventHandler for Handler {
    async fn message(&self, _ctx: Context, msg: DiscordMessage) {
        if msg.author.bot {
            return;
        }

        let sender_id = msg.author.id.to_string();

        if !self.allowed_senders.is_empty() && !self.allowed_senders.contains(&sender_id) {
            return;
        }

        // Download attachments
        let mut media = Vec::new();
        for attachment in &msg.attachments {
            let ext = std::path::Path::new(&attachment.filename)
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| format!(".{e}"))
                .unwrap_or_default();
            let filename = format!("{}{}", attachment.id, ext);
            match download_media(
                &self.download_http,
                &attachment.url,
                &[],
                &self.media_dir,
                &filename,
            )
            .await
            {
                Ok(path) => media.push(path.display().to_string()),
                Err(e) => warn!("failed to download Discord attachment: {e}"),
            }
        }

        let inbound = InboundMessage {
            channel: "discord".into(),
            sender_id,
            chat_id: msg.channel_id.to_string(),
            content: msg.content.clone(),
            timestamp: Utc::now(),
            media,
            metadata: serde_json::json!({
                "message_id": msg.id.to_string(),
                "guild_id": msg.guild_id.map(|g| g.to_string()),
            }),
            message_id: Some(msg.id.to_string()),
        };

        if let Err(e) = self.inbound_tx.send(inbound).await {
            error!("Failed to send Discord inbound message: {e}");
        }
    }

    async fn ready(&self, _ctx: Context, ready: Ready) {
        info!(user = %ready.user.name, "Discord bot connected");
    }
}

#[async_trait]
impl Channel for DiscordChannel {
    fn name(&self) -> &str {
        "discord"
    }

    fn max_message_length(&self) -> usize {
        1900
    }

    fn supports_edit(&self) -> bool {
        true
    }

    async fn start(&self, inbound_tx: mpsc::Sender<InboundMessage>) -> Result<()> {
        info!("Starting Discord channel (gateway)");

        let intents = GatewayIntents::GUILD_MESSAGES
            | GatewayIntents::DIRECT_MESSAGES
            | GatewayIntents::MESSAGE_CONTENT;

        let handler = Handler {
            inbound_tx,
            allowed_senders: self.allowed_senders.clone(),
            media_dir: self.media_dir.clone(),
            download_http: HttpClient::new(),
        };

        let mut client = Client::builder(&self.token, intents)
            .event_handler(handler)
            .await
            .wrap_err("failed to build Discord client")?;

        client.start().await.wrap_err("Discord client error")?;

        info!("Discord channel stopped");
        Ok(())
    }

    async fn send(&self, msg: &OutboundMessage) -> Result<()> {
        self.send_with_id(msg).await?;
        Ok(())
    }

    async fn send_with_id(&self, msg: &OutboundMessage) -> Result<Option<String>> {
        let channel_id: u64 = msg
            .chat_id
            .parse()
            .wrap_err_with(|| format!("invalid Discord channel_id: {}", msg.chat_id))?;

        let channel = serenity::model::id::ChannelId::new(channel_id);

        let sent = if !msg.media.is_empty() {
            let mut attachments = Vec::new();
            for path in &msg.media {
                let attachment = CreateAttachment::path(path)
                    .await
                    .wrap_err_with(|| format!("failed to create Discord attachment: {path}"))?;
                attachments.push(attachment);
            }
            let builder = CreateMessage::new().content(&msg.content);
            channel
                .send_files(&*self.http, attachments, builder)
                .await
                .wrap_err("failed to send Discord message with files")?
        } else {
            channel
                .say(&*self.http, &msg.content)
                .await
                .wrap_err("failed to send Discord message")?
        };

        Ok(Some(sent.id.to_string()))
    }

    async fn edit_message(&self, chat_id: &str, message_id: &str, new_content: &str) -> Result<()> {
        let channel_id: u64 = chat_id
            .parse()
            .wrap_err_with(|| format!("invalid Discord channel_id: {chat_id}"))?;
        let msg_id: u64 = message_id
            .parse()
            .wrap_err_with(|| format!("invalid Discord message_id: {message_id}"))?;

        let channel = serenity::model::id::ChannelId::new(channel_id);
        channel
            .edit_message(
                &*self.http,
                MessageId::new(msg_id),
                EditMessage::new().content(new_content),
            )
            .await
            .wrap_err("failed to edit Discord message")?;

        Ok(())
    }

    async fn delete_message(&self, chat_id: &str, message_id: &str) -> Result<()> {
        let channel_id: u64 = chat_id
            .parse()
            .wrap_err_with(|| format!("invalid Discord channel_id: {chat_id}"))?;
        let msg_id: u64 = message_id
            .parse()
            .wrap_err_with(|| format!("invalid Discord message_id: {message_id}"))?;

        let channel = serenity::model::id::ChannelId::new(channel_id);
        channel
            .delete_message(&*self.http, MessageId::new(msg_id))
            .await
            .wrap_err("failed to delete Discord message")?;

        Ok(())
    }

    fn is_allowed(&self, sender_id: &str) -> bool {
        self.allowed_senders.is_empty() || self.allowed_senders.contains(sender_id)
    }

    async fn stop(&self) -> Result<()> {
        self.shutdown.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_channel(allowed: Vec<&str>) -> DiscordChannel {
        DiscordChannel {
            token: "test.token".into(),
            http: Arc::new(Http::new("test.token")),
            allowed_senders: allowed.into_iter().map(String::from).collect(),
            shutdown: Arc::new(AtomicBool::new(false)),
            media_dir: PathBuf::from("/tmp/test-media"),
        }
    }

    #[test]
    fn test_is_allowed_empty_list() {
        let ch = make_channel(vec![]);
        assert!(ch.is_allowed("anyone"));
    }

    #[test]
    fn test_is_allowed_matching() {
        let ch = make_channel(vec!["12345", "67890"]);
        assert!(ch.is_allowed("12345"));
        assert!(!ch.is_allowed("99999"));
    }

    #[test]
    fn test_is_allowed_not_matching() {
        let ch = make_channel(vec!["12345"]);
        assert!(!ch.is_allowed("other"));
    }

    #[test]
    fn test_channel_name() {
        let ch = make_channel(vec![]);
        assert_eq!(ch.name(), "discord");
    }

    #[test]
    fn test_max_message_length() {
        let ch = make_channel(vec![]);
        assert_eq!(ch.max_message_length(), 1900);
    }
}
