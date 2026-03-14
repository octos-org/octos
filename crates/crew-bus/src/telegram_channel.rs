//! Telegram channel using teloxide long polling.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use crew_core::{InboundMessage, OutboundMessage};
use eyre::{Result, WrapErr};
use reqwest::Client;
use teloxide::prelude::*;
use teloxide::types::{
    BotCommand, ChatId, FileId, InlineKeyboardButton, InlineKeyboardMarkup, InputFile, MessageId,
    ParseMode, ReplyParameters, UpdateKind,
};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::channel::Channel;
use crate::markdown_html::markdown_to_telegram_html;
use crate::media::download_media;

/// Maximum time to wait for a single media download before giving up.
const MEDIA_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30);

/// Telegram caption limit (bytes). We truncate at 1024 chars to be safe.
const CAPTION_MAX_CHARS: usize = 1024;

/// Base delay between reconnection attempts. Doubles on each consecutive failure,
/// capped at `MAX_RECONNECT_DELAY`.
const BASE_RECONNECT_DELAY: Duration = Duration::from_secs(5);

/// Maximum delay between reconnection attempts.
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(60);

pub struct TelegramChannel {
    bot: Bot,
    allowed_senders: HashSet<String>,
    shutdown: Arc<AtomicBool>,
    media_dir: PathBuf,
    http: Client,
    /// Bot username (without @) for mention-gating in groups.
    bot_username: Option<String>,
    /// If true, only respond in group chats when @mentioned or replied to.
    require_mention: bool,
}

impl TelegramChannel {
    pub fn new(
        token: &str,
        allowed_senders: Vec<String>,
        shutdown: Arc<AtomicBool>,
        media_dir: PathBuf,
    ) -> Self {
        Self {
            bot: Bot::new(token),
            allowed_senders: allowed_senders.into_iter().collect(),
            shutdown,
            media_dir,
            http: Client::new(),
            bot_username: None,
            require_mention: false,
        }
    }

    /// Enable mention-gating: bot only responds in groups when @mentioned or replied to.
    pub fn with_mention_gating(mut self, bot_username: Option<String>) -> Self {
        if let Some(username) = bot_username {
            self.bot_username = Some(username);
            self.require_mention = true;
        }
        self
    }

    /// Check if the bot is mentioned in group messages.
    /// Returns `true` if the message should be processed, `false` if it should be ignored.
    fn should_respond_in_group(&self, text: &str, is_group: bool, is_reply_to_bot: bool) -> bool {
        if !is_group || !self.require_mention {
            return true; // DMs always respond; non-gated groups respond to all
        }
        if is_reply_to_bot {
            return true; // Replies to the bot always trigger
        }
        // Check for @mention
        if let Some(ref username) = self.bot_username {
            let mention = format!("@{username}");
            if text.contains(&mention) {
                return true;
            }
        }
        // Check for bot commands (start with /)
        if text.starts_with('/') {
            return true;
        }
        false
    }

    /// Strip the bot @mention from message text.
    fn strip_mention(&self, text: &str) -> String {
        if let Some(ref username) = self.bot_username {
            let mention = format!("@{username}");
            text.replace(&mention, "").trim().to_string()
        } else {
            text.to_string()
        }
    }

    fn check_allowed(&self, sender_id: &str) -> bool {
        if self.allowed_senders.is_empty() {
            return true;
        }
        if self.allowed_senders.contains(sender_id) {
            return true;
        }
        sender_id
            .split('|')
            .any(|part| self.allowed_senders.contains(part))
    }

    /// Download a file from Telegram by file_id, with a timeout.
    async fn download_telegram_file(&self, file_id: &FileId, ext: &str) -> Result<PathBuf> {
        let file = self
            .bot
            .get_file(file_id.clone())
            .await
            .wrap_err("failed to get file info from Telegram")?;

        let url = format!(
            "https://api.telegram.org/file/bot{}/{}",
            self.bot.token(),
            file.path
        );

        let filename = format!("{}{}", file.meta.unique_id, ext);
        download_media(&self.http, &url, &[], &self.media_dir, &filename).await
    }

    /// Try to send an HTML message, falling back to plain text on parse error.
    /// If `reply_to` is provided, the message is threaded as a reply.
    async fn send_html_with_fallback(
        &self,
        chat_id: ChatId,
        html: &str,
        reply_to: Option<MessageId>,
    ) -> Result<Message> {
        let mut req = self.bot.send_message(chat_id, html);
        req = req.parse_mode(ParseMode::Html);
        if let Some(mid) = reply_to {
            req = req.reply_parameters(ReplyParameters::new(mid));
        }
        match req.await {
            Ok(msg) => Ok(msg),
            Err(e) => {
                // Telegram returns "Bad Request: can't parse entities" on malformed HTML.
                let err_str = e.to_string();
                if err_str.contains("can't parse entities") || err_str.contains("parse entities") {
                    warn!("HTML parse failed, falling back to plain text: {e}");
                    let mut fallback = self.bot.send_message(chat_id, html);
                    if let Some(mid) = reply_to {
                        fallback = fallback.reply_parameters(ReplyParameters::new(mid));
                    }
                    fallback
                        .await
                        .wrap_err("failed to send Telegram message (plain text fallback)")
                } else {
                    Err(eyre::eyre!(e).wrap_err("failed to send Telegram message"))
                }
            }
        }
    }

    /// Truncate a caption to Telegram's limit (1024 chars), appending "…" if truncated.
    fn truncate_caption(text: &str) -> String {
        if text.chars().count() <= CAPTION_MAX_CHARS {
            return text.to_string();
        }
        let truncated: String = text.chars().take(CAPTION_MAX_CHARS - 1).collect();
        format!("{truncated}…")
    }

    /// Register bot commands in Telegram's command menu.
    async fn set_commands(&self) {
        let commands = vec![
            BotCommand::new("new", "Start new session or create named session"),
            BotCommand::new("s", "Switch to a named session"),
            BotCommand::new("sessions", "List and switch sessions"),
            BotCommand::new("back", "Switch to previous session"),
            BotCommand::new("delete", "Delete a named session"),
            BotCommand::new(
                "adaptive",
                "View/change adaptive routing (off|hedge|lane|qos)",
            ),
            BotCommand::new(
                "queue",
                "View/change queue mode (followup|collect|steer|interrupt|spec)",
            ),
            BotCommand::new(
                "status",
                "Configure status layers (greeting|provider|metrics|words|add|remove)",
            ),
        ];
        match self.bot.set_my_commands(commands).await {
            Ok(_) => info!("Telegram bot commands registered"),
            Err(e) => warn!("Failed to set bot commands: {e}"),
        }
    }

    /// Parse inline keyboard from OutboundMessage metadata.
    ///
    /// Expected format:
    /// ```json
    /// { "inline_keyboard": [[{"text": "Label", "callback_data": "s:topic"}]] }
    /// ```
    fn parse_inline_keyboard(metadata: &serde_json::Value) -> Option<InlineKeyboardMarkup> {
        let rows = metadata.get("inline_keyboard")?.as_array()?;

        let mut keyboard_rows: Vec<Vec<InlineKeyboardButton>> = Vec::new();
        for row in rows {
            let buttons = row.as_array()?;
            let mut row_buttons = Vec::new();
            for btn in buttons {
                let text = btn.get("text")?.as_str()?;
                let data = btn.get("callback_data")?.as_str()?;
                row_buttons.push(InlineKeyboardButton::callback(
                    text.to_string(),
                    data.to_string(),
                ));
            }
            if !row_buttons.is_empty() {
                keyboard_rows.push(row_buttons);
            }
        }

        if keyboard_rows.is_empty() {
            None
        } else {
            Some(InlineKeyboardMarkup::new(keyboard_rows))
        }
    }
}

#[async_trait]
impl Channel for TelegramChannel {
    fn name(&self) -> &str {
        "telegram"
    }

    fn max_message_length(&self) -> usize {
        4000
    }

    fn supports_edit(&self) -> bool {
        true
    }

    async fn start(&self, inbound_tx: mpsc::Sender<InboundMessage>) -> Result<()> {
        use futures::StreamExt;
        use teloxide::update_listeners::{AsUpdateStream, polling_default};

        info!("Starting Telegram channel (long polling)");

        // Register bot commands in Telegram's command menu
        self.set_commands().await;

        let mut consecutive_failures: u32 = 0;
        // Track the last processed update ID across reconnections to prevent
        // duplicate processing when `polling_default` resets its offset.
        let mut last_update_id: i32 = 0;

        // Outer reconnection loop — restarts polling when the stream ends.
        loop {
            if self.shutdown.load(Ordering::Acquire) {
                break;
            }

            let mut listener = polling_default(self.bot.clone()).await;
            let stream = listener.as_stream();
            tokio::pin!(stream);

            // Reset failure counter on successful stream creation
            consecutive_failures = 0;
            info!(last_update_id, "Telegram polling stream connected");

            while let Some(result) = stream.next().await {
                if self.shutdown.load(Ordering::Acquire) {
                    info!("Telegram channel shutting down (shutdown flag)");
                    return Ok(());
                }

                let update = match result {
                    Ok(update) => update,
                    Err(e) => {
                        warn!("Telegram polling error: {e}");
                        continue;
                    }
                };

                // Dedup: skip updates already processed before reconnection.
                let uid = update.id.0 as i32;
                if uid <= last_update_id {
                    tracing::debug!(
                        update_id = uid,
                        "skipping already-processed Telegram update"
                    );
                    continue;
                }
                last_update_id = uid;

                match update.kind {
                    UpdateKind::Message(msg) => {
                        // Extract text: plain text or caption (for photos/documents)
                        let mut text = msg.text().or(msg.caption()).unwrap_or("").to_string();

                        // Mention-gating: in groups, only respond to @mentions, replies, or commands
                        let is_group = msg.chat.is_group() || msg.chat.is_supergroup();
                        let is_reply_to_bot = msg
                            .reply_to_message()
                            .is_some_and(|r| r.from.as_ref().is_some_and(|u| u.is_bot));
                        if !self.should_respond_in_group(&text, is_group, is_reply_to_bot) {
                            continue;
                        }
                        // Strip @mention from text so the LLM sees clean input
                        if is_group {
                            text = self.strip_mention(&text);
                        }

                        // Download media attachments with timeout so we don't block polling
                        let mut media = Vec::new();

                        if let Some(sizes) = msg.photo() {
                            if let Some(photo) = sizes.last() {
                                match tokio::time::timeout(
                                    MEDIA_DOWNLOAD_TIMEOUT,
                                    self.download_telegram_file(&photo.file.id, ".jpg"),
                                )
                                .await
                                {
                                    Ok(Ok(path)) => media.push(path.display().to_string()),
                                    Ok(Err(e)) => warn!("failed to download photo: {e}"),
                                    Err(_) => warn!(
                                        "photo download timed out after {MEDIA_DOWNLOAD_TIMEOUT:?}"
                                    ),
                                }
                            }
                        }

                        if let Some(voice) = msg.voice() {
                            match tokio::time::timeout(
                                MEDIA_DOWNLOAD_TIMEOUT,
                                self.download_telegram_file(&voice.file.id, ".ogg"),
                            )
                            .await
                            {
                                Ok(Ok(path)) => media.push(path.display().to_string()),
                                Ok(Err(e)) => warn!("failed to download voice: {e}"),
                                Err(_) => warn!(
                                    "voice download timed out after {MEDIA_DOWNLOAD_TIMEOUT:?}"
                                ),
                            }
                        }

                        if let Some(audio) = msg.audio() {
                            let ext = audio
                                .file_name
                                .as_ref()
                                .and_then(|n| std::path::Path::new(n).extension())
                                .map(|e| format!(".{}", e.to_string_lossy()))
                                .unwrap_or_else(|| ".mp3".to_string());
                            match tokio::time::timeout(
                                MEDIA_DOWNLOAD_TIMEOUT,
                                self.download_telegram_file(&audio.file.id, &ext),
                            )
                            .await
                            {
                                Ok(Ok(path)) => media.push(path.display().to_string()),
                                Ok(Err(e)) => warn!("failed to download audio: {e}"),
                                Err(_) => warn!(
                                    "audio download timed out after {MEDIA_DOWNLOAD_TIMEOUT:?}"
                                ),
                            }
                        }

                        if let Some(doc) = msg.document() {
                            let ext = doc
                                .file_name
                                .as_ref()
                                .and_then(|n| std::path::Path::new(n).extension())
                                .map(|e| format!(".{}", e.to_string_lossy()))
                                .unwrap_or_default();
                            match tokio::time::timeout(
                                MEDIA_DOWNLOAD_TIMEOUT,
                                self.download_telegram_file(&doc.file.id, &ext),
                            )
                            .await
                            {
                                Ok(Ok(path)) => media.push(path.display().to_string()),
                                Ok(Err(e)) => warn!("failed to download document: {e}"),
                                Err(_) => warn!(
                                    "document download timed out after {MEDIA_DOWNLOAD_TIMEOUT:?}"
                                ),
                            }
                        }

                        // Skip messages with no text and no media
                        if text.is_empty() && media.is_empty() {
                            continue;
                        }

                        let sender_id = msg
                            .from
                            .as_ref()
                            .map(|u| {
                                let id = u.id.to_string();
                                match &u.username {
                                    Some(name) => format!("{id}|{name}"),
                                    None => id,
                                }
                            })
                            .unwrap_or_default();

                        if !self.check_allowed(&sender_id) {
                            continue;
                        }

                        let inbound = InboundMessage {
                            channel: "telegram".into(),
                            sender_id,
                            chat_id: msg.chat.id.0.to_string(),
                            content: text,
                            timestamp: Utc::now(),
                            media,
                            metadata: serde_json::json!({}),
                            message_id: Some(msg.id.0.to_string()),
                        };

                        if inbound_tx.send(inbound).await.is_err() {
                            info!("Inbound channel closed, stopping Telegram listener");
                            return Ok(());
                        }
                    }

                    UpdateKind::CallbackQuery(cb) => {
                        // Dismiss the loading spinner on the button
                        if let Err(e) = self.bot.answer_callback_query(cb.id.clone()).await {
                            warn!("Failed to answer callback query: {e}");
                        }

                        let sender_id = {
                            let id = cb.from.id.to_string();
                            match &cb.from.username {
                                Some(name) => format!("{id}|{name}"),
                                None => id,
                            }
                        };

                        if !self.check_allowed(&sender_id) {
                            continue;
                        }

                        // Extract chat_id and message_id from the callback's source message
                        let (chat_id, message_id) = match &cb.message {
                            Some(mim) => {
                                (mim.chat().id.0.to_string(), Some(mim.id().0.to_string()))
                            }
                            None => continue,
                        };

                        let callback_data = cb.data.unwrap_or_default();

                        let inbound = InboundMessage {
                            channel: "telegram".into(),
                            sender_id,
                            chat_id,
                            content: callback_data.clone(),
                            timestamp: Utc::now(),
                            media: vec![],
                            metadata: serde_json::json!({
                                "callback_query": true,
                                "callback_data": callback_data,
                                "callback_message_id": message_id,
                            }),
                            message_id: None,
                        };

                        if inbound_tx.send(inbound).await.is_err() {
                            info!("Inbound channel closed, stopping Telegram listener");
                            return Ok(());
                        }
                    }

                    _ => {} // Ignore other update kinds
                }
            }

            // Stream ended (returned None). Check if we should reconnect.
            if self.shutdown.load(Ordering::Acquire) {
                break;
            }

            consecutive_failures += 1;
            let delay = std::cmp::min(
                BASE_RECONNECT_DELAY * 2u32.saturating_pow(consecutive_failures - 1),
                MAX_RECONNECT_DELAY,
            );
            warn!(
                consecutive_failures,
                delay_secs = delay.as_secs(),
                "Telegram polling stream ended unexpectedly, reconnecting..."
            );
            tokio::time::sleep(delay).await;
        }

        info!("Telegram channel stopped");
        Ok(())
    }

    async fn send(&self, msg: &OutboundMessage) -> Result<()> {
        let chat_id: i64 = msg
            .chat_id
            .parse()
            .wrap_err_with(|| format!("invalid Telegram chat_id: {}", msg.chat_id))?;

        let reply_to = msg
            .reply_to
            .as_deref()
            .and_then(|id| id.parse::<i32>().ok())
            .map(MessageId);

        if !msg.media.is_empty() {
            // Send files as documents
            let caption = if msg.content.is_empty() {
                None
            } else {
                let html = markdown_to_telegram_html(&msg.content);
                Some(Self::truncate_caption(&html))
            };

            for (i, path) in msg.media.iter().enumerate() {
                let file_path = std::path::PathBuf::from(path);
                let file_size = std::fs::metadata(&file_path).map(|m| m.len()).unwrap_or(0);
                info!(path, size = file_size, "sending media file via Telegram");
                let file = InputFile::file(&file_path);
                let lower = path.to_lowercase();

                if lower.ends_with(".ogg") || lower.ends_with(".oga") || lower.ends_with(".opus") {
                    // Send as voice message (shows as audio bubble in Telegram)
                    let mut req = self.bot.send_voice(ChatId(chat_id), file);
                    if i == 0 {
                        if let Some(ref cap) = caption {
                            req = req.caption(cap).parse_mode(ParseMode::Html);
                        }
                        if let Some(mid) = reply_to {
                            req = req.reply_parameters(ReplyParameters::new(mid));
                        }
                    }
                    req.await
                        .wrap_err_with(|| format!("failed to send voice: {path}"))?;
                } else if lower.ends_with(".mp3")
                    || lower.ends_with(".wav")
                    || lower.ends_with(".m4a")
                {
                    // Send as audio file (shows with player controls)
                    let mut req = self.bot.send_audio(ChatId(chat_id), file);
                    if i == 0 {
                        if let Some(ref cap) = caption {
                            req = req.caption(cap).parse_mode(ParseMode::Html);
                        }
                        if let Some(mid) = reply_to {
                            req = req.reply_parameters(ReplyParameters::new(mid));
                        }
                    }
                    req.await
                        .wrap_err_with(|| format!("failed to send audio: {path}"))?;
                } else {
                    // Send as document (generic file)
                    let mut req = self.bot.send_document(ChatId(chat_id), file);
                    if i == 0 {
                        if let Some(ref cap) = caption {
                            req = req.caption(cap).parse_mode(ParseMode::Html);
                        }
                        if let Some(mid) = reply_to {
                            req = req.reply_parameters(ReplyParameters::new(mid));
                        }
                    }
                    req.await
                        .wrap_err_with(|| format!("failed to send document: {path}"))?;
                }
            }
        } else {
            let html = markdown_to_telegram_html(&msg.content);

            // Check for inline keyboard in metadata
            if let Some(markup) = Self::parse_inline_keyboard(&msg.metadata) {
                let mut req = self.bot.send_message(ChatId(chat_id), &html);
                req = req.parse_mode(ParseMode::Html).reply_markup(markup);
                if let Some(mid) = reply_to {
                    req = req.reply_parameters(ReplyParameters::new(mid));
                }
                req.await
                    .wrap_err("failed to send Telegram message with keyboard")?;
            } else {
                self.send_html_with_fallback(ChatId(chat_id), &html, reply_to)
                    .await?;
            }
        }

        Ok(())
    }

    fn is_allowed(&self, sender_id: &str) -> bool {
        self.check_allowed(sender_id)
    }

    async fn stop(&self) -> Result<()> {
        self.shutdown.store(true, Ordering::Release);
        Ok(())
    }

    async fn send_typing(&self, chat_id: &str) -> Result<()> {
        let id: i64 = chat_id
            .parse()
            .wrap_err_with(|| format!("invalid Telegram chat_id: {chat_id}"))?;
        self.bot
            .send_chat_action(ChatId(id), teloxide::types::ChatAction::Typing)
            .await
            .wrap_err("failed to send typing action")?;
        Ok(())
    }

    async fn send_listening(&self, chat_id: &str) -> Result<()> {
        let id: i64 = chat_id
            .parse()
            .wrap_err_with(|| format!("invalid Telegram chat_id: {chat_id}"))?;
        self.bot
            .send_chat_action(ChatId(id), teloxide::types::ChatAction::RecordVoice)
            .await
            .wrap_err("failed to send record_voice action")?;
        Ok(())
    }

    async fn send_with_id(&self, msg: &OutboundMessage) -> Result<Option<String>> {
        let chat_id: i64 = msg
            .chat_id
            .parse()
            .wrap_err_with(|| format!("invalid Telegram chat_id: {}", msg.chat_id))?;

        let reply_to = msg
            .reply_to
            .as_deref()
            .and_then(|id| id.parse::<i32>().ok())
            .map(MessageId);

        let html = markdown_to_telegram_html(&msg.content);

        let sent = if let Some(markup) = Self::parse_inline_keyboard(&msg.metadata) {
            let mut req = self.bot.send_message(ChatId(chat_id), &html);
            req = req.parse_mode(ParseMode::Html).reply_markup(markup);
            if let Some(mid) = reply_to {
                req = req.reply_parameters(ReplyParameters::new(mid));
            }
            req.await
                .wrap_err("failed to send Telegram message with keyboard")?
        } else {
            self.send_html_with_fallback(ChatId(chat_id), &html, reply_to)
                .await?
        };

        Ok(Some(sent.id.0.to_string()))
    }

    async fn edit_message(&self, chat_id: &str, message_id: &str, new_content: &str) -> Result<()> {
        let cid: i64 = chat_id
            .parse()
            .wrap_err_with(|| format!("invalid Telegram chat_id: {chat_id}"))?;
        let mid: i32 = message_id
            .parse()
            .wrap_err_with(|| format!("invalid Telegram message_id: {message_id}"))?;

        let html = markdown_to_telegram_html(new_content);
        self.bot
            .edit_message_text(ChatId(cid), MessageId(mid), &html)
            .parse_mode(ParseMode::Html)
            .await
            .wrap_err("failed to edit Telegram message")?;
        Ok(())
    }

    async fn delete_message(&self, chat_id: &str, message_id: &str) -> Result<()> {
        let cid: i64 = chat_id
            .parse()
            .wrap_err_with(|| format!("invalid Telegram chat_id: {chat_id}"))?;
        let mid: i32 = message_id
            .parse()
            .wrap_err_with(|| format!("invalid Telegram message_id: {message_id}"))?;

        self.bot
            .delete_message(ChatId(cid), MessageId(mid))
            .await
            .wrap_err("failed to delete Telegram message")?;
        Ok(())
    }

    async fn edit_message_with_metadata(
        &self,
        chat_id: &str,
        message_id: &str,
        new_content: &str,
        metadata: &serde_json::Value,
    ) -> Result<()> {
        let cid: i64 = chat_id
            .parse()
            .wrap_err_with(|| format!("invalid Telegram chat_id: {chat_id}"))?;
        let mid: i32 = message_id
            .parse()
            .wrap_err_with(|| format!("invalid Telegram message_id: {message_id}"))?;

        let html = markdown_to_telegram_html(new_content);

        if let Some(markup) = Self::parse_inline_keyboard(metadata) {
            self.bot
                .edit_message_text(ChatId(cid), MessageId(mid), &html)
                .parse_mode(ParseMode::Html)
                .reply_markup(markup)
                .await
                .wrap_err("failed to edit Telegram message with keyboard")?;
        } else {
            self.bot
                .edit_message_text(ChatId(cid), MessageId(mid), &html)
                .parse_mode(ParseMode::Html)
                .await
                .wrap_err("failed to edit Telegram message")?;
        }
        Ok(())
    }

    // NOTE: Telegram's send() already converts Markdown → HTML internally,
    // so format_outbound() is intentionally not overridden here (uses default pass-through).
    // This avoids double-conversion when the dispatcher calls format_outbound() + send().

    async fn health_check(&self) -> Result<crate::channel::ChannelHealth> {
        match self.bot.get_me().await {
            Ok(_) => Ok(crate::channel::ChannelHealth::Healthy),
            Err(e) => Ok(crate::channel::ChannelHealth::Down(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_channel(allowed: Vec<&str>) -> TelegramChannel {
        TelegramChannel {
            bot: Bot::new("test:token"),
            allowed_senders: allowed.into_iter().map(String::from).collect(),
            shutdown: Arc::new(AtomicBool::new(false)),
            media_dir: PathBuf::from("/tmp/test-media"),
            http: Client::new(),
            bot_username: None,
            require_mention: false,
        }
    }

    fn make_channel_with_mention_gating(allowed: Vec<&str>, bot_username: &str) -> TelegramChannel {
        TelegramChannel {
            bot: Bot::new("test:token"),
            allowed_senders: allowed.into_iter().map(String::from).collect(),
            shutdown: Arc::new(AtomicBool::new(false)),
            media_dir: PathBuf::from("/tmp/test-media"),
            http: Client::new(),
            bot_username: Some(bot_username.to_string()),
            require_mention: true,
        }
    }

    #[test]
    fn test_is_allowed_empty_list() {
        let ch = make_channel(vec![]);
        assert!(ch.is_allowed("anyone"));
        assert!(ch.is_allowed("12345"));
    }

    #[test]
    fn test_is_allowed_matching() {
        let ch = make_channel(vec!["12345", "67890"]);
        assert!(ch.is_allowed("12345"));
        assert!(ch.is_allowed("67890"));
        assert!(!ch.is_allowed("99999"));
    }

    #[test]
    fn test_is_allowed_compound_id() {
        let ch = make_channel(vec!["12345", "johndoe"]);
        assert!(ch.is_allowed("12345|johndoe"));
        assert!(ch.is_allowed("12345|other"));
        assert!(ch.is_allowed("99999|johndoe"));
        assert!(!ch.is_allowed("99999|other"));
    }

    #[test]
    fn test_truncate_caption_short() {
        let short = "Hello world";
        assert_eq!(TelegramChannel::truncate_caption(short), short);
    }

    #[test]
    fn test_truncate_caption_long() {
        let long: String = "x".repeat(2000);
        let truncated = TelegramChannel::truncate_caption(&long);
        assert_eq!(truncated.chars().count(), CAPTION_MAX_CHARS);
        assert!(truncated.ends_with('…'));
    }

    #[test]
    fn test_truncate_caption_exact() {
        let exact: String = "a".repeat(CAPTION_MAX_CHARS);
        assert_eq!(TelegramChannel::truncate_caption(&exact), exact);
    }

    #[test]
    fn test_channel_name() {
        let ch = make_channel(vec![]);
        assert_eq!(ch.name(), "telegram");
    }

    #[test]
    fn test_max_message_length() {
        let ch = make_channel(vec![]);
        assert_eq!(ch.max_message_length(), 4000);
    }

    #[test]
    fn test_parse_inline_keyboard_valid() {
        let meta = serde_json::json!({
            "inline_keyboard": [[
                {"text": "Option A", "callback_data": "a"},
                {"text": "Option B", "callback_data": "b"}
            ]]
        });
        let kb = TelegramChannel::parse_inline_keyboard(&meta);
        assert!(kb.is_some());
        let kb = kb.unwrap();
        assert_eq!(kb.inline_keyboard.len(), 1);
        assert_eq!(kb.inline_keyboard[0].len(), 2);
    }

    #[test]
    fn test_parse_inline_keyboard_multiple_rows() {
        let meta = serde_json::json!({
            "inline_keyboard": [
                [{"text": "Row1", "callback_data": "r1"}],
                [{"text": "Row2", "callback_data": "r2"}]
            ]
        });
        let kb = TelegramChannel::parse_inline_keyboard(&meta).unwrap();
        assert_eq!(kb.inline_keyboard.len(), 2);
    }

    #[test]
    fn test_parse_inline_keyboard_missing() {
        let meta = serde_json::json!({});
        assert!(TelegramChannel::parse_inline_keyboard(&meta).is_none());
    }

    #[test]
    fn test_parse_inline_keyboard_empty_rows() {
        let meta = serde_json::json!({"inline_keyboard": []});
        assert!(TelegramChannel::parse_inline_keyboard(&meta).is_none());
    }

    #[test]
    fn test_parse_inline_keyboard_missing_fields() {
        // Buttons missing callback_data should cause None
        let meta = serde_json::json!({
            "inline_keyboard": [[{"text": "Label"}]]
        });
        assert!(TelegramChannel::parse_inline_keyboard(&meta).is_none());
    }

    #[test]
    fn test_truncate_caption_multibyte() {
        // Ensure truncation works with multi-byte UTF-8 characters
        let text: String = "\u{1F600}".repeat(CAPTION_MAX_CHARS + 10);
        let truncated = TelegramChannel::truncate_caption(&text);
        assert_eq!(truncated.chars().count(), CAPTION_MAX_CHARS);
        assert!(truncated.ends_with('\u{2026}')); // ellipsis
    }

    // ── Mention-gating tests ───────────────────────────────────────────

    #[test]
    fn should_respond_in_dm_without_mention() {
        let ch = make_channel_with_mention_gating(vec![], "MyBot");
        assert!(ch.should_respond_in_group("hello", false, false));
    }

    #[test]
    fn should_ignore_group_message_without_mention() {
        let ch = make_channel_with_mention_gating(vec![], "MyBot");
        assert!(!ch.should_respond_in_group("hello everyone", true, false));
    }

    #[test]
    fn should_respond_in_group_when_mentioned() {
        let ch = make_channel_with_mention_gating(vec![], "MyBot");
        assert!(ch.should_respond_in_group("@MyBot what is this?", true, false));
    }

    #[test]
    fn should_respond_in_group_when_replied_to() {
        let ch = make_channel_with_mention_gating(vec![], "MyBot");
        assert!(ch.should_respond_in_group("yes please", true, true));
    }

    #[test]
    fn should_respond_in_group_for_commands() {
        let ch = make_channel_with_mention_gating(vec![], "MyBot");
        assert!(ch.should_respond_in_group("/new topic", true, false));
    }

    #[test]
    fn should_respond_in_group_when_gating_disabled() {
        let ch = make_channel(vec![]);
        assert!(ch.should_respond_in_group("random message", true, false));
    }

    #[test]
    fn should_strip_mention_from_text() {
        let ch = make_channel_with_mention_gating(vec![], "MyBot");
        assert_eq!(ch.strip_mention("@MyBot summarize this"), "summarize this");
        assert_eq!(ch.strip_mention("hello @MyBot"), "hello");
        assert_eq!(ch.strip_mention("no mention here"), "no mention here");
    }
}
