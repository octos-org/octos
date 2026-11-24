//! The Telegram transport of `serve.im` (T-0663, `docs/IM_REMOTE_CONTROL.md`
//! §9): a bot's messages in, the bridge's replies out.
//!
//! One loop per bot asks `getUpdates` for what Telegram has not been told
//! is done, gives each message to the bridge, and only then asks from the
//! next offset, which is what tells Telegram. By then the bridge has
//! recorded the message's turn, or answered it. So after a restart, a lost
//! connection, or an update Telegram sends again, the bridge finds the turn
//! that already ran and runs nothing. The offset lives with Telegram, which
//! keeps an update it was not told about for 24 hours; the bridge's time to
//! live refuses anything older than it allows.
//!
//! The loop asks from an offset only right after a batch, to tell Telegram
//! it is done; any other poll, after an empty one or a failure, asks from
//! Telegram's earliest update it was not told about. After a week with no
//! updates Telegram numbers the next one at random, possibly below the last
//! offset, and asking from that offset would confirm it unseen. At worst a
//! batch whose confirmation was lost comes again, and runs nothing again.
//!
//! A reply goes out in pieces Telegram accepts, as HTML when it formats,
//! the first piece as a reply to the message. A send Telegram did not
//! answer may still have arrived, so it is not tried again and the bridge
//! takes the result as not confirmed. The one send repeated is one Telegram
//! refused for its HTML: that one did not arrive, and goes again as text.

use octos_bus::coalesce::{ChunkConfig, split_message};
use octos_bus::markdown_html::markdown_to_telegram_html;
use octos_bus::telegram_api::{ApiError, Message, TelegramBotApi};

use super::im_bridge::{ImChannel, ImChannelId, ImMessage, ImSender, ServeImBridge};
use super::*;

/// How long one `getUpdates` waits for a message; under the channels' HTTP
/// read timeout.
const WAIT: Duration = Duration::from_secs(25);
/// The wait after a failed `getUpdates`, doubled on each failure in a row.
const BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

pub(crate) struct TelegramBot {
    id: ImChannelId,
    api: TelegramBotApi,
}

impl TelegramBot {
    pub(crate) fn new(id: ImChannelId, api: TelegramBotApi) -> Self {
        Self { id, api }
    }
}

#[async_trait::async_trait]
impl ImChannel for TelegramBot {
    fn id(&self) -> ImChannelId {
        self.id.clone()
    }

    async fn send(&self, chat: i64, text: &str, reply_to: Option<i64>) -> eyre::Result<()> {
        for (index, piece) in split_message(text, &ChunkConfig::telegram())
            .iter()
            .enumerate()
        {
            let reply_to = reply_to.filter(|_| index == 0);
            let html = markdown_to_telegram_html(piece);
            match self.api.send_message(chat, &html, true, reply_to).await {
                Err(ApiError::Refused { description, .. })
                    if description.contains("can't parse entities") =>
                {
                    self.api.send_message(chat, piece, false, reply_to).await?
                }
                sent => sent?,
            }
        }
        Ok(())
    }
}

/// The bridge's reading of `message`, as the platform tells it. A message
/// without text has other content (a photo, a file, a sticker, a place, a
/// service note), which the bridge runs none of.
pub(crate) fn im_message(message: &Message) -> ImMessage {
    let sender = match (&message.sender_chat, &message.from) {
        (Some(chat), _) => ImSender::Chat(chat.id),
        (None, Some(user)) if user.is_bot => ImSender::Bot(user.id),
        (None, Some(user)) => ImSender::Person(user.id),
        (None, None) => ImSender::Chat(message.chat.id),
    };
    ImMessage {
        chat: message.chat.id,
        direct: message.chat.kind == "private",
        sender,
        message_id: Some(message.message_id),
        sent_at: DateTime::from_timestamp(message.date, 0),
        text: (message.text.clone())
            .or_else(|| message.caption.clone())
            .unwrap_or_default(),
        attachments: usize::from(message.text.is_none()),
    }
}

/// Give `bot`'s messages to `bridge` until aborted.
pub(crate) async fn poll(bridge: Arc<ServeImBridge>, bot: Arc<TelegramBot>) {
    // After the last batch, the offset that confirms it; else 0.
    let mut offset = 0;
    let mut backoff = BACKOFF;
    loop {
        let asked = std::mem::take(&mut offset);
        let updates = match bot.api.get_updates(asked, WAIT).await {
            Ok(updates) => updates,
            Err(error) => {
                warn!(bot = bot.id.bot, %error, "serve.im: Telegram getUpdates failed; asking again");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
        };
        backoff = BACKOFF;
        for update in updates {
            if let Some(message) = &update.message {
                // A turn this starts runs on and replies by itself.
                drop(bridge.handle(im_message(message)).await);
            }
            offset = offset.max(update.update_id + 1);
        }
    }
}

#[cfg(test)]
#[path = "ui_protocol_im_telegram_tests.rs"]
mod tests;
