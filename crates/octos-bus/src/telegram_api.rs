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
    /// The API answered and refused: nothing took effect.
    Refused { code: i64, description: String },
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unanswered(why) => write!(f, "no answer from Telegram: {why}"),
            Self::Refused { code, description } => {
                write!(f, "Telegram refused ({code}): {description}")
            }
        }
    }
}

impl std::error::Error for ApiError {}

#[derive(Debug, Clone, Deserialize)]
pub struct Update {
    pub update_id: i64,
    #[serde(default)]
    pub message: Option<Message>,
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
        let body =
            json!({ "offset": offset, "timeout": wait.as_secs(), "allowed_updates": ["message"] });
        let result = self.call("getUpdates", body).await?;
        serde_json::from_value(result).map_err(|error| {
            ApiError::Unanswered(format!("getUpdates: unreadable updates: {error}"))
        })
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
        let mut body = json!({ "chat_id": chat, "text": text });
        if html {
            body["parse_mode"] = json!("HTML");
        }
        if let Some(message_id) = reply_to {
            body["reply_parameters"] =
                json!({ "message_id": message_id, "allow_sending_without_reply": true });
        }
        self.call("sendMessage", body).await.map(drop)
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
            }),
            _ => Err(unanswered(format!("HTTP {status} without an API answer"))),
        }
    }

    fn redact(&self, text: String) -> String {
        text.replace(&self.token, "<token>")
    }
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
