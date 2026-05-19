//! LINE channel with Webhook mode + REST API.
//!
//! Creates a local server to receive events via webhook and sending messages
//! back to LINE using the Messaging API.
//! Requires specifying a LINE Channel Access Token to send messages back to
//! LINE, and a LINE Channel Secret to properly verify incoming webhook events.

use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use async_trait::async_trait;
use axum::{Router, extract::State, http::HeaderMap, response::IntoResponse, routing::post};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use chrono::Utc;
use eyre::{Result, WrapErr};
use octos_core::{InboundMessage, OutboundMessage};
use reqwest::Client;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::channel::Channel;
use crate::coalesce::{ChunkConfig, split_message};
use crate::dedup::MessageDedup;
use crate::media::{download_media, is_image};

const LINE_API_BASE: &str = "https://api.line.me";
const LINE_DATA_API_BASE: &str = "https://api-data.line.me";

/// HMAC-SHA256 for LINE webhook signature validation
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK_SIZE: usize = 64;

    let mut key = if key.len() > BLOCK_SIZE {
        Sha256::digest(key).to_vec()
    } else {
        key.to_vec()
    };
    key.resize(BLOCK_SIZE, 0);

    let mut ipad = [0x36u8; BLOCK_SIZE];
    let mut opad = [0x5cu8; BLOCK_SIZE];
    for i in 0..BLOCK_SIZE {
        ipad[i] ^= key[i];
        opad[i] ^= key[i];
    }

    let mut inner = ipad.to_vec();
    inner.extend_from_slice(message);
    let inner_hash = Sha256::digest(&inner);

    let mut outer = opad.to_vec();
    outer.extend_from_slice(&inner_hash);
    Sha256::digest(&outer).into()
}

/// Map a UTF-16 code-unit range (LINE mention indices) to byte offsets in `text`.
fn utf16_range_to_byte_range(
    text: &str,
    utf16_start: usize,
    utf16_len: usize,
) -> Option<(usize, usize)> {
    let mut utf16_pos = 0usize;
    let mut byte_start = None;
    for (byte_idx, ch) in text.char_indices() {
        if utf16_pos == utf16_start {
            byte_start = Some(byte_idx);
        }
        if utf16_pos == utf16_start + utf16_len {
            return Some((byte_start?, byte_idx));
        }
        utf16_pos += ch.len_utf16();
    }
    if utf16_pos == utf16_start + utf16_len {
        return Some((byte_start?, text.len()));
    }
    None
}

/// Verify `X-Line-Signature` per LINE Messaging API webhook docs
fn verify_line_signature(channel_secret: &str, body: &str, signature: &str) -> bool {
    if signature.is_empty() {
        return false;
    }
    let digest = hmac_sha256(channel_secret.as_bytes(), body.as_bytes());
    let computed = BASE64.encode(digest);
    computed.as_bytes().ct_eq(signature.as_bytes()).into()
}

pub struct LineChannel {
    channel_secret: String,
    channel_access_token: String,
    allowed_senders: HashSet<String>,
    shutdown: Arc<AtomicBool>,
    http: Client,
    media_dir: PathBuf,
    webhook_port: u16,
    dedup: MessageDedup,
    /// Bot user ID (U…) for mention-gating in groups/rooms.
    bot_user_id: Mutex<Option<String>>,
    /// If true, only respond in group/room chats when @mentioned or sent a /command.
    require_mention: bool,
}

impl LineChannel {
    pub fn new(
        channel_secret: &str,
        channel_access_token: &str,
        allowed_senders: Vec<String>,
        shutdown: Arc<AtomicBool>,
        media_dir: PathBuf,
    ) -> Self {
        Self {
            channel_secret: channel_secret.to_string(),
            channel_access_token: channel_access_token.to_string(),
            allowed_senders: allowed_senders.into_iter().collect(),
            shutdown,
            http: Client::new(),
            media_dir,
            webhook_port: 8646,
            dedup: MessageDedup::new(),
            bot_user_id: Mutex::new(None),
            require_mention: false,
        }
    }

    pub fn with_webhook_port(mut self, port: u16) -> Self {
        self.webhook_port = port;
        self
    }

    /// Enable mention-gating: bot only responds in groups/rooms when @mentioned or sent a /command.
    pub fn with_mention_gating(mut self, bot_user_id: Option<String>) -> Self {
        self.require_mention = true;
        if let Some(id) = bot_user_id {
            *self.bot_user_id.lock().expect("bot_user_id lock") = Some(id);
        }
        self
    }

    /// Resolve bot user ID from settings or LINE `GET /v2/bot/info`.
    async fn ensure_bot_user_id(&self) -> Result<()> {
        if !self.require_mention {
            return Ok(());
        }
        if self.bot_user_id.lock().expect("bot_user_id lock").is_some() {
            return Ok(());
        }
        let id = self.fetch_bot_user_id().await?;
        *self.bot_user_id.lock().expect("bot_user_id lock") = Some(id);
        Ok(())
    }

    async fn fetch_bot_user_id(&self) -> Result<String> {
        let resp: serde_json::Value = self
            .http
            .get(format!("{LINE_API_BASE}/v2/bot/info"))
            .header("Authorization", self.auth_header())
            .send()
            .await
            .wrap_err("LINE bot info request failed")?
            .json()
            .await?;

        resp.get("userId")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| eyre::eyre!("LINE bot info response missing userId: {resp}"))
    }

    fn bot_user_id(&self) -> Option<String> {
        self.bot_user_id.lock().expect("bot_user_id lock").clone()
    }

    fn is_group_source(source_type: &str) -> bool {
        matches!(source_type, "group" | "room")
    }

    /// Whether the bot appears in the message `mention.mentionees` list.
    fn is_bot_mentioned(&self, message: &serde_json::Value) -> bool {
        let Some(bot_id) = self.bot_user_id() else {
            return false;
        };
        let Some(mentionees) = message
            .get("mention")
            .and_then(|m| m.get("mentionees"))
            .and_then(|v| v.as_array())
        else {
            return false;
        };
        mentionees.iter().any(|m| {
            m.get("type").and_then(|v| v.as_str()) == Some("all")
                || m.get("userId").and_then(|v| v.as_str()) == Some(bot_id.as_str())
        })
    }

    /// Check if the bot is mentioned in group/room messages.
    fn should_respond_in_group(
        &self,
        source_type: &str,
        text: &str,
        message: &serde_json::Value,
    ) -> bool {
        if !Self::is_group_source(source_type) || !self.require_mention {
            return true;
        }
        if self.is_bot_mentioned(message) {
            return true;
        }
        if text.starts_with('/') {
            return true;
        }
        false
    }

    /// Remove @mention spans for this bot from text (LINE indices are UTF-16 code units).
    fn strip_bot_mentions(&self, text: &str, message: &serde_json::Value) -> String {
        let Some(bot_id) = self.bot_user_id() else {
            return text.to_string();
        };
        let Some(mentionees) = message
            .get("mention")
            .and_then(|m| m.get("mentionees"))
            .and_then(|v| v.as_array())
        else {
            return text.to_string();
        };

        let mut ranges: Vec<(usize, usize)> = mentionees
            .iter()
            .filter_map(|m| {
                if m.get("userId").and_then(|v| v.as_str()) != Some(bot_id.as_str()) {
                    return None;
                }
                let utf16_start = m.get("index").and_then(|v| v.as_u64())? as usize;
                let utf16_len = m.get("length").and_then(|v| v.as_u64())? as usize;
                utf16_range_to_byte_range(text, utf16_start, utf16_len)
            })
            .collect();
        ranges.sort_by(|a, b| b.0.cmp(&a.0));

        let mut result = text.to_string();
        for (start, end) in ranges {
            if end <= result.len() {
                result.replace_range(start..end, "");
            }
        }
        result.trim().to_string()
    }

    fn check_allowed(&self, sender_id: &str) -> bool {
        self.allowed_senders.is_empty() || self.allowed_senders.contains(sender_id)
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.channel_access_token)
    }

    /// Resolve chat_id (push `to`) and sender_id from a webhook event `source`
    fn source_ids(source: &serde_json::Value) -> Option<(String, String)> {
        let source_type = source.get("type").and_then(|v| v.as_str())?;
        let user_id = source.get("userId").and_then(|v| v.as_str()).unwrap_or("");

        let chat_id = match source_type {
            "user" => user_id,
            "group" => source.get("groupId").and_then(|v| v.as_str())?,
            "room" => source.get("roomId").and_then(|v| v.as_str())?,
            _ => return None,
        };

        if chat_id.is_empty() || user_id.is_empty() {
            return None;
        }

        Some((chat_id.to_string(), user_id.to_string()))
    }

    /// Download binary content for a user-sent message
    async fn download_line_content(&self, message_id: &str, ext: &str) -> Result<PathBuf> {
        let url = format!("{LINE_DATA_API_BASE}/v2/bot/message/{message_id}/content");
        let filename = format!("line_{}{ext}", Utc::now().timestamp_millis());
        download_media(
            &self.http,
            &url,
            &[("Authorization", &self.auth_header())],
            &self.media_dir,
            &filename,
        )
        .await
    }

    /// Upload local media for outbound push/reply (image/audio/video)
    async fn upload_content(&self, file_path: &str, content_type: &str) -> Result<String> {
        let data = std::fs::read(file_path).wrap_err("failed to read file for LINE upload")?;
        let filename = std::path::Path::new(file_path)
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| "file".to_string());

        let part = reqwest::multipart::Part::bytes(data)
            .file_name(filename)
            .mime_str("application/octet-stream")?;
        let form = reqwest::multipart::Form::new()
            .text("type", content_type.to_string())
            .part("file", part);

        let resp: serde_json::Value = self
            .http
            .post(format!("{LINE_DATA_API_BASE}/v2/bot/message/upload"))
            .header("Authorization", self.auth_header())
            .multipart(form)
            .send()
            .await
            .wrap_err("LINE content upload request failed")?
            .json()
            .await?;

        resp.get("contentId")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| eyre::eyre!("LINE upload response missing contentId: {resp}"))
    }

    /// Parse a single webhook event into an inbound message.
    async fn parse_event(&self, event: &serde_json::Value) -> Option<InboundMessage> {
        if event.get("type").and_then(|v| v.as_str()) != Some("message") {
            return None;
        }

        let message = event.get("message")?;
        let message_id = message.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if message_id.is_empty() || self.dedup.is_duplicate(message_id) {
            debug!(message_id, "LINE: dedup filtered message");
            return None;
        }

        let source = event.get("source")?;
        let (chat_id, sender_id) = Self::source_ids(source)?;
        if !self.check_allowed(&sender_id) {
            return None;
        }

        let source_type = source.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let msg_type = message.get("type").and_then(|v| v.as_str()).unwrap_or("");

        if Self::is_group_source(source_type) && self.require_mention {
            let text_preview = message.get("text").and_then(|v| v.as_str()).unwrap_or("");
            if !self.should_respond_in_group(source_type, text_preview, message) {
                debug!(
                    source_type,
                    msg_type, "LINE: ignored group message (mention gating)"
                );
                return None;
            }
        }

        let mut content = String::new();
        let mut media = Vec::new();

        match msg_type {
            "text" => {
                let text = message.get("text").and_then(|v| v.as_str()).unwrap_or("");
                content = if Self::is_group_source(source_type) {
                    self.strip_bot_mentions(text, message)
                } else {
                    text.to_string()
                };
            }
            "image" => match self.download_line_content(message_id, ".jpg").await {
                Ok(path) => media.push(path.display().to_string()),
                Err(e) => warn!("failed to download LINE image: {e}"),
            },
            "audio" => match self.download_line_content(message_id, ".m4a").await {
                Ok(path) => media.push(path.display().to_string()),
                Err(e) => warn!("failed to download LINE audio: {e}"),
            },
            "video" => match self.download_line_content(message_id, ".mp4").await {
                Ok(path) => media.push(path.display().to_string()),
                Err(e) => warn!("failed to download LINE video: {e}"),
            },
            "file" => {
                let file_name = message
                    .get("fileName")
                    .and_then(|v| v.as_str())
                    .unwrap_or("file");
                let ext = std::path::Path::new(file_name)
                    .extension()
                    .map(|e| format!(".{}", e.to_string_lossy()))
                    .unwrap_or_default();
                match self.download_line_content(message_id, &ext).await {
                    Ok(path) => media.push(path.display().to_string()),
                    Err(e) => warn!("failed to download LINE file: {e}"),
                }
            }
            "location" => {
                let title = message
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Location");
                let address = message
                    .get("address")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let lat = message
                    .get("latitude")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let lng = message
                    .get("longitude")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                content = format!("{title}\n{address}\n({lat}, {lng})");
            }
            "sticker" => {
                content = "[sticker message]".to_string();
            }
            _ => {
                content = format!("[{msg_type} message]");
            }
        }

        if content.is_empty() && media.is_empty() {
            debug!(
                message_id,
                msg_type, "LINE: empty content and media, skipping"
            );
            return None;
        }

        let reply_token = event
            .get("replyToken")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        info!(
            message_id,
            msg_type,
            media_count = media.len(),
            "LINE: parsed event"
        );

        Some(InboundMessage {
            channel: "line".into(),
            sender_id,
            chat_id,
            content,
            timestamp: Utc::now(),
            media,
            metadata: serde_json::json!({
                "line": {
                    "message_id": message_id,
                    "message_type": msg_type,
                    "reply_token": reply_token,
                    "source_type": event
                        .get("source")
                        .and_then(|s| s.get("type"))
                        .and_then(|v| v.as_str()),
                }
            }),
            message_id: Some(message_id.to_string()),
        })
    }

    fn build_text_messages(content: &str) -> Vec<serde_json::Value> {
        let config = ChunkConfig { max_chars: 5000 };
        split_message(content, &config)
            .into_iter()
            .map(|chunk| serde_json::json!({"type": "text", "text": chunk}))
            .collect()
    }

    async fn push_messages(&self, to: &str, messages: Vec<serde_json::Value>) -> Result<()> {
        if messages.is_empty() {
            return Ok(());
        }

        let body = serde_json::json!({
            "to": to,
            "messages": messages,
        });

        let resp = self
            .http
            .post(format!("{LINE_API_BASE}/v2/bot/message/push"))
            .header("Authorization", self.auth_header())
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .wrap_err("LINE push request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            warn!("LINE push error (HTTP {status}): {body}");
        }

        Ok(())
    }

    /// Run webhook HTTP server mode
    async fn start_webhook(&self, inbound_tx: mpsc::Sender<InboundMessage>) -> Result<()> {
        if let Err(e) = self.ensure_bot_user_id().await {
            if self.require_mention {
                warn!("LINE: could not resolve bot user ID for mention gating: {e}");
            }
        }
        #[derive(Clone)]
        struct WebhookState {
            channel_secret: String,
            inbound_tx: mpsc::Sender<serde_json::Value>,
        }

        async fn handle_webhook(
            State(state): State<WebhookState>,
            headers: HeaderMap,
            body: String,
        ) -> impl IntoResponse {
            let signature = headers
                .get("X-Line-Signature")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");

            if !verify_line_signature(&state.channel_secret, &body, signature) {
                warn!("LINE webhook: signature mismatch");
                return (
                    axum::http::StatusCode::FORBIDDEN,
                    axum::Json(serde_json::json!({"error": "signature mismatch"})),
                )
                    .into_response();
            }

            let payload: serde_json::Value = match serde_json::from_str(&body) {
                Ok(v) => v,
                Err(e) => {
                    warn!("LINE webhook: invalid JSON body: {e}");
                    return (
                        axum::http::StatusCode::BAD_REQUEST,
                        axum::Json(serde_json::json!({"error": "invalid json"})),
                    )
                        .into_response();
                }
            };

            let _ = state.inbound_tx.send(payload).await;
            "ok".into_response()
        }

        let (event_tx, mut event_rx) = mpsc::channel::<serde_json::Value>(100);

        let state = WebhookState {
            channel_secret: self.channel_secret.clone(),
            inbound_tx: event_tx,
        };

        let app = Router::new()
            .route("/line/webhook", post(handle_webhook))
            .with_state(state);

        let addr = format!("0.0.0.0:{}", self.webhook_port);
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .wrap_err_with(|| format!("failed to bind LINE webhook server to {addr}"))?;
        info!(port = self.webhook_port, "LINE webhook server listening");

        let shutdown = self.shutdown.clone();
        let server_shutdown = shutdown.clone();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    while !server_shutdown.load(Ordering::Relaxed) {
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                })
                .await
                .ok();
        });

        while let Some(payload) = event_rx.recv().await {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }

            let events = payload
                .get("events")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();

            for event in events {
                if let Some(inbound) = self.parse_event(&event).await {
                    info!(
                        sender = %inbound.sender_id,
                        chat = %inbound.chat_id,
                        "LINE: sending to inbound bus"
                    );
                    if inbound_tx.send(inbound).await.is_err() {
                        error!("LINE: inbound_tx send failed (receiver dropped)");
                        return Ok(());
                    }
                }
            }
        }

        Ok(())
    }
}

#[async_trait]
impl Channel for LineChannel {
    fn name(&self) -> &str {
        "line"
    }

    async fn start(&self, inbound_tx: mpsc::Sender<InboundMessage>) -> Result<()> {
        info!(port = self.webhook_port, "Starting LINE channel");
        self.start_webhook(inbound_tx).await?;
        info!("LINE channel stopped");
        Ok(())
    }

    async fn send(&self, msg: &OutboundMessage) -> Result<()> {
        let mut messages = Self::build_text_messages(&msg.content);

        for path in &msg.media {
            if is_image(path) {
                match self.upload_content(path, "image").await {
                    Ok(content_id) => {
                        messages.push(serde_json::json!({
                            "type": "image",
                            "id": content_id,
                        }));
                    }
                    Err(e) => warn!("failed to upload LINE image: {e}"),
                }
            } else {
                warn!(path = %path, "LINE: non-image media not supported for outbound send");
            }
        }

        // LINE allows up to 5 messages per push/reply request
        for chunk in messages.chunks(5) {
            self.push_messages(&msg.chat_id, chunk.to_vec()).await?;
        }

        Ok(())
    }

    fn is_allowed(&self, sender_id: &str) -> bool {
        self.check_allowed(sender_id)
    }

    fn max_message_length(&self) -> usize {
        5000
    }

    async fn stop(&self) -> Result<()> {
        self.shutdown.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_channel(allowed: Vec<&str>) -> LineChannel {
        LineChannel {
            channel_secret: "secret".into(),
            channel_access_token: "token".into(),
            allowed_senders: allowed.into_iter().map(String::from).collect(),
            shutdown: Arc::new(AtomicBool::new(false)),
            http: Client::new(),
            media_dir: PathBuf::from("/tmp/test-line-media"),
            webhook_port: 8646,
            dedup: MessageDedup::new(),
            bot_user_id: Mutex::new(None),
            require_mention: false,
        }
    }

    fn make_channel_with_mention_gating(allowed: Vec<&str>, bot_user_id: &str) -> LineChannel {
        make_channel(allowed).with_mention_gating(Some(bot_user_id.to_string()))
    }

    #[test]
    fn test_verify_line_signature() {
        let body = r#"{"events":[]}"#;
        let digest = hmac_sha256(b"secret", body.as_bytes());
        let sig = BASE64.encode(digest);
        assert!(verify_line_signature("secret", body, &sig));
        assert!(!verify_line_signature("secret", body, "invalid"));
    }

    #[test]
    fn test_source_ids_user() {
        let source = serde_json::json!({
            "type": "user",
            "userId": "U123"
        });
        let (chat, sender) = LineChannel::source_ids(&source).unwrap();
        assert_eq!(chat, "U123");
        assert_eq!(sender, "U123");
    }

    #[test]
    fn test_source_ids_group() {
        let source = serde_json::json!({
            "type": "group",
            "groupId": "G123",
            "userId": "U456"
        });
        let (chat, sender) = LineChannel::source_ids(&source).unwrap();
        assert_eq!(chat, "G123");
        assert_eq!(sender, "U456");
    }

    #[test]
    fn test_is_allowed() {
        let ch = make_channel(vec!["U1"]);
        assert!(ch.is_allowed("U1"));
        assert!(!ch.is_allowed("U2"));
        let open = make_channel(vec![]);
        assert!(open.is_allowed("anyone"));
    }

    #[test]
    fn should_respond_in_dm_without_mention() {
        let ch = make_channel_with_mention_gating(vec![], "U_bot");
        let msg = serde_json::json!({"type": "text", "text": "hello"});
        assert!(ch.should_respond_in_group("user", "hello", &msg));
    }

    #[test]
    fn should_ignore_unmentioned_group_message() {
        let ch = make_channel_with_mention_gating(vec![], "U_bot");
        let msg = serde_json::json!({"type": "text", "text": "hello everyone"});
        assert!(!ch.should_respond_in_group("group", "hello everyone", &msg));
    }

    #[test]
    fn should_respond_in_group_when_mentioned() {
        let ch = make_channel_with_mention_gating(vec![], "U_bot");
        let msg = serde_json::json!({
            "type": "text",
            "text": "hi @Bot",
            "mention": {
                "mentionees": [{
                    "index": 3,
                    "length": 4,
                    "userId": "U_bot"
                }]
            }
        });
        assert!(ch.should_respond_in_group("group", "hi @Bot", &msg));
    }

    #[test]
    fn should_respond_in_group_for_commands() {
        let ch = make_channel_with_mention_gating(vec![], "U_bot");
        let msg = serde_json::json!({"type": "text", "text": "/help"});
        assert!(ch.should_respond_in_group("group", "/help", &msg));
    }

    #[test]
    fn should_respond_in_group_when_gating_disabled() {
        let ch = make_channel(vec![]);
        let msg = serde_json::json!({"type": "text", "text": "random"});
        assert!(ch.should_respond_in_group("group", "random", &msg));
    }

    #[test]
    fn strip_bot_mentions_removes_mention_span() {
        let ch = make_channel_with_mention_gating(vec![], "U_bot");
        let msg = serde_json::json!({
            "type": "text",
            "text": "hi @Bot please help",
            "mention": {
                "mentionees": [{
                    "index": 3,
                    "length": 4,
                    "userId": "U_bot"
                }]
            }
        });
        assert_eq!(
            ch.strip_bot_mentions("hi @Bot please help", &msg),
            "hi  please help"
        );
    }

    #[tokio::test]
    async fn should_preserve_message_id_from_inbound_event() {
        let ch = make_channel(vec![]);
        let event = serde_json::json!({
            "type": "message",
            "replyToken": "reply-token",
            "source": {
                "type": "user",
                "userId": "U_user1"
            },
            "message": {
                "type": "text",
                "id": "msg_abc123",
                "text": "hello"
            }
        });

        let inbound = ch.parse_event(&event).await.expect("should parse");
        assert_eq!(
            inbound.message_id,
            Some("msg_abc123".to_string()),
            "LINE must preserve platform message_id"
        );
        assert_eq!(inbound.chat_id, "U_user1");
        assert_eq!(inbound.content, "hello");
    }
}
