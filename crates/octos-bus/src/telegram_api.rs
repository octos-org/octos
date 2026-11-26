//! The Telegram Bot API's `getUpdates` and `sendMessage`, on the channels'
//! HTTP client, for a consumer that confirms each update itself (T-0663,
//! `docs/IM_REMOTE_CONTROL.md` §9).
//!
//! [`TelegramChannel`](crate::telegram_channel) polls through teloxide, which
//! confirms an update as soon as it has handed it on, and it turns a message
//! into an `InboundMessage` without the chat's type, the sender's kind or
//! the platform's time. `octos serve`'s IM bridge needs all three, and
//! confirms an update only once it has recorded what it did with it, so it
//! polls here. Nothing here needs the `telegram` feature.
//!
//! The bot's token is in every request path, so no URL leaves this module:
//! an error says what failed without one, and the client does not print its
//! token.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

/// Telegram's own Bot API server.
pub const API: &str = "https://api.telegram.org";

pub struct TelegramBotApi {
    http: reqwest::Client,
    /// `<api>/bot<token>`.
    base: String,
    token: String,
}

impl std::fmt::Debug for TelegramBotApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TelegramBotApi { .. }")
    }
}

/// Why a call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// No answer from the API: the request may still have taken effect.
    Unanswered(String),
    /// The API answered and refused: nothing took effect. Asked to slow
    /// down (429), it says how long to wait first.
    Refused {
        code: i64,
        description: String,
        retry_after: Option<Duration>,
    },
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unanswered(why) => write!(f, "no answer from Telegram: {why}"),
            Self::Refused {
                code, description, ..
            } => write!(f, "Telegram refused ({code}): {description}"),
        }
    }
}

impl std::error::Error for ApiError {}

#[derive(Debug, Clone, Deserialize)]
pub struct Update {
    pub update_id: i64,
    #[serde(default)]
    pub message: Option<Message>,
    #[serde(default)]
    pub callback_query: Option<CallbackQuery>,
}

/// A button press on a message the bot sent.
#[derive(Debug, Clone, Deserialize)]
pub struct CallbackQuery {
    /// The platform's id of the press, which `answerCallbackQuery` wants.
    pub id: String,
    #[serde(default)]
    pub message: Option<Message>,
    #[serde(default)]
    pub from: Option<User>,
    #[serde(default)]
    pub data: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub message_id: i64,
    /// When the platform says it was sent, in Unix seconds.
    pub date: i64,
    pub chat: Chat,
    #[serde(default)]
    pub from: Option<User>,
    /// The chat the message was sent in the name of: an anonymous group
    /// admin's, or a channel's.
    #[serde(default)]
    pub sender_chat: Option<Chat>,
    /// Set on a text message only.
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub caption: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Chat {
    pub id: i64,
    /// `private`, `group`, `supergroup` or `channel`.
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub id: i64,
    pub is_bot: bool,
}

impl TelegramBotApi {
    /// The bot with `token`, on the Bot API server at `api`.
    pub fn new(api: &str, token: &str) -> Self {
        Self {
            http: crate::http_client::channel_http_client(),
            base: format!("{}/bot{token}", api.trim_end_matches('/')),
            token: token.to_owned(),
        }
    }

    /// The messages not confirmed yet, from `offset` on, waiting up to
    /// `wait` for one. Asking from an offset confirms every update before
    /// it, so pass an update's id plus one only once done with it.
    pub async fn get_updates(&self, offset: i64, wait: Duration) -> Result<Vec<Update>, ApiError> {
        let body = json!({ "offset": offset, "timeout": wait.as_secs(), "allowed_updates": ["message", "callback_query"] });
        let Value::Array(updates) = self.call("getUpdates", body).await? else {
            return Err(ApiError::Unanswered(
                "getUpdates: the result is not a list".into(),
            ));
        };
        Ok(updates.into_iter().filter_map(read_update).collect())
    }

    /// Send `text` to `chat`, as HTML if `html`, as a reply to `reply_to`
    /// while that message still exists.
    pub async fn send_message(
        &self,
        chat: i64,
        text: &str,
        html: bool,
        reply_to: Option<i64>,
    ) -> Result<(), ApiError> {
        self.send_message_keyboard(chat, text, html, reply_to, &[])
            .await
            .map(drop)
    }

    /// [`Self::send_message`] with an inline keyboard (one `(label, callback
    /// data)` pair per button, one array per row), returning the sent
    /// message's id so it can be edited later. Telegram caps callback data
    /// at 64 bytes; the caller's data is far below that.
    pub async fn send_message_keyboard(
        &self,
        chat: i64,
        text: &str,
        html: bool,
        reply_to: Option<i64>,
        keyboard: &[Vec<(String, String)>],
    ) -> Result<i64, ApiError> {
        let mut body = json!({ "chat_id": chat, "text": text });
        if html {
            body["parse_mode"] = json!("HTML");
        }
        if let Some(message_id) = reply_to {
            body["reply_parameters"] =
                json!({ "message_id": message_id, "allow_sending_without_reply": true });
        }
        if !keyboard.is_empty() {
            let rows: Vec<Vec<Value>> = keyboard
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|(text, data)| json!({ "text": text, "callback_data": data }))
                        .collect::<Vec<Value>>()
                })
                .collect();
            body["reply_markup"] = json!({ "inline_keyboard": rows });
        }
        let result = self.call("sendMessage", body).await?;
        result["message_id"]
            .as_i64()
            .ok_or_else(|| ApiError::Unanswered("sendMessage: no message id in the answer".into()))
    }

    /// Edit a message the bot sent, dropping its keyboard (an answered or
    /// expired approval's buttons must not stay tappable).
    pub async fn edit_message_text(
        &self,
        chat: i64,
        message_id: i64,
        text: &str,
        html: bool,
    ) -> Result<(), ApiError> {
        let mut body = json!({
            "chat_id": chat,
            "message_id": message_id,
            "text": text,
            "reply_markup": { "inline_keyboard": [] },
        });
        if html {
            body["parse_mode"] = json!("HTML");
        }
        self.call("editMessageText", body).await.map(drop)
    }

    /// Answer a button press; `alert` shows `text` as a popup instead of a
    /// toast. An unanswered answer leaves Telegram's spinner running, which
    /// tells the sender nothing — callers report the server state instead.
    pub async fn answer_callback_query(
        &self,
        callback_id: &str,
        text: Option<&str>,
        alert: bool,
    ) -> Result<(), ApiError> {
        let mut body = json!({ "callback_query_id": callback_id, "show_alert": alert });
        if let Some(text) = text {
            body["text"] = json!(text);
        }
        self.call("answerCallbackQuery", body).await.map(drop)
    }

    async fn call(&self, method: &str, body: Value) -> Result<Value, ApiError> {
        let unanswered =
            |why: String| ApiError::Unanswered(self.redact(format!("{method}: {why}")));
        let response = self
            .http
            .post(format!("{}/{method}", self.base))
            .json(&body)
            .send()
            .await
            .map_err(|error| unanswered(describe(error)))?;
        let status = response.status();
        let answer: Value = response
            .json()
            .await
            .map_err(|_| unanswered(format!("HTTP {status} without an API answer")))?;
        if answer["ok"] == json!(true) {
            return Ok(answer["result"].clone());
        }
        match (
            answer["error_code"].as_i64(),
            answer["description"].as_str(),
        ) {
            (Some(code), Some(description)) => Err(ApiError::Refused {
                code,
                description: self.redact(format!("{method}: {description}")),
                retry_after: answer["parameters"]["retry_after"]
                    .as_u64()
                    .map(Duration::from_secs),
            }),
            _ => Err(unanswered(format!("HTTP {status} without an API answer"))),
        }
    }

    fn redact(&self, text: String) -> String {
        text.replace(&self.token, "<token>")
    }
}

/// One update as Telegram sent it. One that does not read as an update is
/// kept by its id without its message, so it is confirmed and skipped
/// instead of holding up the updates after it (T-0665). Only its id is
/// logged: the rest may be someone's message.
fn read_update(update: Value) -> Option<Update> {
    let update_id = update["update_id"].as_i64();
    serde_json::from_value(update).ok().or_else(|| {
        tracing::warn!(?update_id, "Telegram update unreadable; skipped");
        Some(Update {
            update_id: update_id?,
            message: None,
            callback_query: None,
        })
    })
}

/// What went wrong with a request that got no answer, without its URL.
fn describe(error: reqwest::Error) -> String {
    let kind = if error.is_timeout() {
        "timed out"
    } else if error.is_connect() {
        "could not connect"
    } else {
        "request failed"
    };
    let error = error.without_url();
    let mut why = format!("{kind}: {error}");
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        why.push_str(&format!(": {cause}"));
        source = cause.source();
    }
    why
}
