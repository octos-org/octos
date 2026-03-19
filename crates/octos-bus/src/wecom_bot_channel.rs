//! WeCom Group Robot (群机器人) channel — WebSocket long connection mode.
//!
//! Connects to `wss://openws.work.weixin.qq.com` using BotID + Secret.
//! Receives messages via `aibot_msg_callback` frames pushed over the WebSocket.
//! Replies via `aibot_send_msg` (markdown) over the same WebSocket.
//! No public callback URL required — the bot connects outbound.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use chrono::Utc;
use eyre::{Result, WrapErr, bail};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use octos_core::{InboundMessage, OutboundMessage};
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

// Re-used for explicit TLS connector (avoids CryptoProvider auto-detection panic).
extern crate rustls;
extern crate rustls_native_certs;

use crate::channel::Channel;

/// Default WeCom WebSocket endpoint.
const WS_URL: &str = "wss://openws.work.weixin.qq.com";
/// Ping interval in seconds.
const PING_INTERVAL_SECS: u64 = 30;
/// Max missed heartbeat ACKs before force reconnect.
const MAX_MISSED_HEARTBEATS: u32 = 2;
/// Max reconnection attempts.
const MAX_RECONNECT_ATTEMPTS: u32 = 100;
/// Base reconnect delay in milliseconds.
const RECONNECT_BASE_DELAY_MS: u64 = 5000;
/// Max reconnect delay in milliseconds.
const RECONNECT_MAX_DELAY_MS: u64 = 60000;
/// Maximum message IDs to track for dedup.
const MAX_SEEN_IDS: usize = 1000;
/// Max message length for WeCom markdown.
const MAX_MSG_LENGTH: usize = 4096;

type WsSink = SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    WsMessage,
>;

/// WeCom Group Robot channel (WebSocket long connection).
///
/// - Authenticates via `aibot_subscribe` with BotID + Secret
/// - Receives messages via `aibot_msg_callback` frames
/// - Sends replies via `aibot_send_msg` (markdown) over WebSocket
/// - Heartbeat via `ping`/`pong` every 30s
/// - Auto-reconnects with exponential backoff
pub struct WeComBotChannel {
    bot_id: String,
    secret: String,
    allowed_senders: HashSet<String>,
    shutdown: Arc<AtomicBool>,
    seen_ids: Arc<std::sync::Mutex<HashSet<String>>>,
    /// Shared write-half of the WebSocket, set once connected.
    ws_sink: Arc<Mutex<Option<WsSink>>>,
}

impl WeComBotChannel {
    pub fn new(
        bot_id: &str,
        secret: &str,
        allowed_senders: Vec<String>,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        Self {
            bot_id: bot_id.to_string(),
            secret: secret.to_string(),
            allowed_senders: allowed_senders.into_iter().collect(),
            shutdown,
            seen_ids: Arc::new(std::sync::Mutex::new(HashSet::new())),
            ws_sink: Arc::new(Mutex::new(None)),
        }
    }

    fn check_allowed(&self, sender_id: &str) -> bool {
        self.allowed_senders.is_empty() || self.allowed_senders.contains(sender_id)
    }

    /// Check if a message ID has been seen; add if not.
    fn dedup_check(&self, msg_id: &str) -> bool {
        let mut seen = self.seen_ids.lock().unwrap_or_else(|e| e.into_inner());
        if seen.contains(msg_id) {
            return true;
        }
        if seen.len() >= MAX_SEEN_IDS {
            seen.clear();
        }
        seen.insert(msg_id.to_string());
        false
    }

    /// Parse an `aibot_msg_callback` body into an InboundMessage.
    fn parse_callback(&self, body: &Value) -> Option<InboundMessage> {
        let msg_type = body.get("msgtype").and_then(|v| v.as_str())?;
        let from_user = body
            .get("from")
            .and_then(|f| f.get("userid"))
            .and_then(|v| v.as_str())?;
        let msg_id = body
            .get("msgid")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let chat_id = body
            .get("chatid")
            .and_then(|v| v.as_str())
            .unwrap_or(from_user);

        if !msg_id.is_empty() && self.dedup_check(msg_id) {
            debug!(msg_id, "WeComBot: dedup filtered message");
            return None;
        }

        if !self.check_allowed(from_user) {
            debug!(from_user, "WeComBot: sender not allowed");
            return None;
        }

        let content = match msg_type {
            "text" => body
                .get("text")
                .and_then(|t| t.get("content"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim()
                .to_string(),
            "mixed" => {
                // Extract text parts from mixed message items
                let items = body
                    .get("mixed")
                    .and_then(|m| m.get("msg_item"))
                    .and_then(|v| v.as_array());
                match items {
                    Some(arr) => arr
                        .iter()
                        .filter_map(|item| {
                            let t = item.get("msgtype").and_then(|v| v.as_str())?;
                            if t == "text" {
                                item.get("text")
                                    .and_then(|t| t.get("content"))
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string())
                            } else {
                                Some(format!("[{t}]"))
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" "),
                    None => "[mixed message]".to_string(),
                }
            }
            "image" => "[image]".to_string(),
            "voice" => body
                .get("voice")
                .and_then(|v| v.get("content"))
                .and_then(|v| v.as_str())
                .unwrap_or("[voice]")
                .to_string(),
            "file" => "[file]".to_string(),
            "event" => {
                let event = body
                    .get("event")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                debug!(event, "WeComBot: received event");
                return None;
            }
            _ => {
                debug!(msg_type, "WeComBot: unsupported message type");
                return None;
            }
        };

        if content.is_empty() {
            return None;
        }

        info!(
            msg_id,
            msg_type, from_user, chat_id, "WeComBot: parsed message"
        );

        Some(InboundMessage {
            channel: "wecom-bot".into(),
            sender_id: from_user.to_string(),
            chat_id: chat_id.to_string(),
            content,
            timestamp: Utc::now(),
            media: vec![],
            metadata: json!({
                "wecom_bot": {
                    "msg_id": msg_id,
                    "msg_type": msg_type,
                }
            }),
            message_id: Some(msg_id.to_string()),
        })
    }

    /// Build the `aibot_subscribe` frame.
    fn subscribe_frame(&self) -> String {
        json!({
            "cmd": "aibot_subscribe",
            "headers": {
                "req_id": format!("aibot_subscribe_{}", Uuid::now_v7()),
            },
            "body": {
                "bot_id": self.bot_id,
                "secret": self.secret,
            }
        })
        .to_string()
    }

    /// Build a ping frame.
    fn ping_frame() -> String {
        json!({
            "cmd": "ping",
            "headers": {
                "req_id": format!("ping_{}", Uuid::now_v7()),
            }
        })
        .to_string()
    }

    /// Build an `aibot_send_msg` frame.
    fn send_msg_frame(chat_id: &str, content: &str) -> String {
        json!({
            "cmd": "aibot_send_msg",
            "headers": {
                "req_id": format!("send_{}", Uuid::now_v7()),
            },
            "body": {
                "chatid": chat_id,
                "msgtype": "markdown",
                "markdown": {
                    "content": content,
                }
            }
        })
        .to_string()
    }

    /// Build a TLS connector for the WebSocket connection.
    fn make_tls_connector() -> Result<tokio_tungstenite::Connector> {
        // Explicitly build a rustls ClientConfig to avoid the CryptoProvider
        // auto-detection panic when both `ring` and `aws-lc-rs` are present.
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut root_store = rustls::RootCertStore::empty();
        for cert in rustls_native_certs::load_native_certs().certs {
            root_store.add(cert).ok();
        }
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .wrap_err("WeComBot: failed to configure TLS")?
            .with_root_certificates(root_store)
            .with_no_client_auth();
        Ok(tokio_tungstenite::Connector::Rustls(Arc::new(config)))
    }

    /// Connect, subscribe, and process messages. Returns on disconnect/error.
    async fn run_connection(&self, inbound_tx: &mpsc::Sender<InboundMessage>) -> Result<()> {
        info!("WeComBot: connecting to {WS_URL}");

        let connector = Self::make_tls_connector()?;
        let (ws_stream, _) =
            tokio_tungstenite::connect_async_tls_with_config(WS_URL, None, false, Some(connector))
                .await
                .wrap_err("WeComBot: failed to connect WebSocket")?;

        info!("WeComBot: WebSocket connected");

        let (mut sink, stream) = ws_stream.split();

        // Send subscribe
        let sub_frame = self.subscribe_frame();
        sink.send(WsMessage::Text(sub_frame.into()))
            .await
            .wrap_err("WeComBot: failed to send subscribe")?;

        info!("WeComBot: subscribe sent, waiting for ACK");

        // Store sink for outbound sending
        {
            let mut ws = self.ws_sink.lock().await;
            *ws = Some(sink);
        }

        // Process incoming frames
        let result = self.process_frames(stream, inbound_tx).await;

        // Clear sink on disconnect
        {
            let mut ws = self.ws_sink.lock().await;
            *ws = None;
        }

        result
    }

    /// Read frames from the WebSocket stream.
    async fn process_frames(
        &self,
        mut stream: SplitStream<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
        >,
        inbound_tx: &mpsc::Sender<InboundMessage>,
    ) -> Result<()> {
        let mut missed_heartbeats: u32 = 0;
        let mut subscribed = false;
        let mut ping_interval =
            tokio::time::interval(std::time::Duration::from_secs(PING_INTERVAL_SECS));

        loop {
            if self.shutdown.load(Ordering::Acquire) {
                info!("WeComBot: shutdown requested");
                return Ok(());
            }

            tokio::select! {
                _ = ping_interval.tick() => {
                    if !subscribed {
                        continue;
                    }
                    missed_heartbeats += 1;
                    if missed_heartbeats > MAX_MISSED_HEARTBEATS {
                        warn!("WeComBot: too many missed heartbeats, reconnecting");
                        bail!("missed heartbeats");
                    }

                    let frame = Self::ping_frame();
                    let mut ws = self.ws_sink.lock().await;
                    if let Some(ref mut sink) = *ws {
                        if let Err(e) = sink.send(WsMessage::Text(frame.into())).await {
                            warn!("WeComBot: ping send failed: {e}");
                            bail!("ping send failed");
                        }
                    }
                }
                msg = stream.next() => {
                    match msg {
                        Some(Ok(WsMessage::Text(text))) => {
                            let text_str: &str = &text;
                            missed_heartbeats = 0;

                            match serde_json::from_str::<Value>(text_str) {
                                Ok(frame) => {
                                    let cmd = frame.get("cmd").and_then(|v| v.as_str()).unwrap_or("");

                                    match cmd {
                                        "aibot_msg_callback" => {
                                            if let Some(body) = frame.get("body") {
                                                if let Some(inbound) = self.parse_callback(body) {
                                                    if inbound_tx.send(inbound).await.is_err() {
                                                        error!("WeComBot: inbound_tx dropped");
                                                        bail!("inbound channel closed");
                                                    }
                                                }
                                            }
                                        }
                                        "aibot_event_callback" => {
                                            info!("WeComBot: event frame: {}", serde_json::to_string(&frame).unwrap_or_default());
                                            if let Some(body) = frame.get("body") {
                                                let event = body.get("event")
                                                    .and_then(|v| v.as_str())
                                                    .unwrap_or("unknown");
                                                info!(event, "WeComBot: event callback");

                                                if event == "disconnected_event" {
                                                    warn!("WeComBot: displaced by another connection");
                                                    bail!("disconnected by server (displaced)");
                                                }
                                            }
                                        }
                                        "pong" => {
                                            debug!("WeComBot: pong received");
                                        }
                                        "" => {
                                            // ACK frame (no cmd, has errcode)
                                            let errcode = frame.get("errcode")
                                                .and_then(|v| v.as_i64())
                                                .unwrap_or(-1);

                                            if !subscribed {
                                                if errcode == 0 {
                                                    subscribed = true;
                                                    info!("WeComBot: subscribed successfully");
                                                } else {
                                                    let errmsg = frame.get("errmsg")
                                                        .and_then(|v| v.as_str())
                                                        .unwrap_or("unknown");
                                                    error!(errcode, errmsg, "WeComBot: subscribe failed");
                                                    bail!("subscribe failed: {errmsg} (code {errcode})");
                                                }
                                            } else if errcode != 0 {
                                                let errmsg = frame.get("errmsg")
                                                    .and_then(|v| v.as_str())
                                                    .unwrap_or("unknown");
                                                warn!(errcode, errmsg, "WeComBot: server error");
                                            }
                                        }
                                        other => {
                                            info!(cmd = other, "WeComBot: unknown command, frame: {}", serde_json::to_string(&frame).unwrap_or_default());
                                        }
                                    }
                                }
                                Err(e) => {
                                    warn!("WeComBot: malformed JSON frame: {e}");
                                }
                            }
                        }
                        Some(Ok(WsMessage::Ping(data))) => {
                            let mut ws = self.ws_sink.lock().await;
                            if let Some(ref mut sink) = *ws {
                                let _ = sink.send(WsMessage::Pong(data)).await;
                            }
                        }
                        Some(Ok(WsMessage::Close(_))) => {
                            info!("WeComBot: server closed connection");
                            bail!("server closed connection");
                        }
                        Some(Err(e)) => {
                            warn!("WeComBot: WebSocket error: {e}");
                            bail!("WebSocket error: {e}");
                        }
                        None => {
                            info!("WeComBot: WebSocket stream ended");
                            bail!("stream ended");
                        }
                        _ => {} // Binary, Frame — ignore
                    }
                }
            }
        }
    }

    /// Main loop: connect, process, reconnect with exponential backoff.
    async fn run_loop(&self, inbound_tx: mpsc::Sender<InboundMessage>) {
        let mut attempts: u32 = 0;

        loop {
            if self.shutdown.load(Ordering::Acquire) {
                break;
            }

            let started = std::time::Instant::now();

            match self.run_connection(&inbound_tx).await {
                Ok(()) => {
                    // Clean shutdown
                    break;
                }
                Err(e) => {
                    // If the connection was alive for >30s it was a healthy session
                    // that disconnected, not a connect-time failure.  Reset the
                    // counter so cumulative disconnects over days don't hit the cap.
                    if started.elapsed().as_secs() > 30 {
                        attempts = 0;
                    }

                    attempts += 1;
                    if attempts > MAX_RECONNECT_ATTEMPTS {
                        error!("WeComBot: max reconnect attempts reached, giving up");
                        break;
                    }

                    let delay = std::cmp::min(
                        RECONNECT_BASE_DELAY_MS * 2u64.saturating_pow(attempts - 1),
                        RECONNECT_MAX_DELAY_MS,
                    );
                    warn!(
                        error = %e,
                        attempt = attempts,
                        delay_ms = delay,
                        "WeComBot: connection lost, reconnecting"
                    );

                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
            }
        }
    }
}

#[async_trait]
impl Channel for WeComBotChannel {
    fn name(&self) -> &str {
        "wecom-bot"
    }

    async fn start(&self, inbound_tx: mpsc::Sender<InboundMessage>) -> Result<()> {
        info!("Starting WeComBot channel (WebSocket long connection)");
        self.run_loop(inbound_tx).await;
        info!("WeComBot channel stopped");
        Ok(())
    }

    async fn send(&self, msg: &OutboundMessage) -> Result<()> {
        if msg.content.is_empty() {
            return Ok(());
        }

        info!(
            chat_id = %msg.chat_id,
            content_len = msg.content.len(),
            "WeComBot: sending message"
        );

        let frame = Self::send_msg_frame(&msg.chat_id, &msg.content);
        let mut ws = self.ws_sink.lock().await;
        match *ws {
            Some(ref mut sink) => {
                sink.send(WsMessage::Text(frame.into()))
                    .await
                    .wrap_err("WeComBot: failed to send message")?;
            }
            None => {
                bail!("WeComBot: WebSocket not connected, cannot send");
            }
        }

        if !msg.media.is_empty() {
            warn!(
                count = msg.media.len(),
                "WeComBot: file attachments not supported for group robot, skipping"
            );
        }

        Ok(())
    }

    fn is_allowed(&self, sender_id: &str) -> bool {
        self.check_allowed(sender_id)
    }

    fn max_message_length(&self) -> usize {
        MAX_MSG_LENGTH
    }

    async fn stop(&self) -> Result<()> {
        self.shutdown.store(true, Ordering::Release);
        // Close the WebSocket if connected
        let mut ws = self.ws_sink.lock().await;
        if let Some(ref mut sink) = *ws {
            let _ = sink.close().await;
        }
        *ws = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_bot(allowed: Vec<&str>) -> WeComBotChannel {
        WeComBotChannel {
            bot_id: "test_bot_id".into(),
            secret: "test_secret".into(),
            allowed_senders: allowed.into_iter().map(String::from).collect(),
            shutdown: Arc::new(AtomicBool::new(false)),
            seen_ids: Arc::new(std::sync::Mutex::new(HashSet::new())),
            ws_sink: Arc::new(Mutex::new(None)),
        }
    }

    #[test]
    fn should_return_wecom_bot_as_channel_name() {
        let bot = make_bot(vec![]);
        assert_eq!(bot.name(), "wecom-bot");
    }

    #[test]
    fn should_return_4096_as_max_message_length() {
        let bot = make_bot(vec![]);
        assert_eq!(bot.max_message_length(), MAX_MSG_LENGTH);
    }

    #[test]
    fn should_allow_anyone_when_empty_list() {
        let bot = make_bot(vec![]);
        assert!(bot.is_allowed("anyone"));
    }

    #[test]
    fn should_deny_unlisted_sender_when_allow_list_set() {
        let bot = make_bot(vec!["user1", "user2"]);
        assert!(bot.is_allowed("user1"));
        assert!(!bot.is_allowed("user3"));
    }

    #[test]
    fn should_detect_duplicate_messages() {
        let bot = make_bot(vec![]);
        assert!(!bot.dedup_check("msg1"));
        assert!(bot.dedup_check("msg1"));
        assert!(!bot.dedup_check("msg2"));
    }

    #[test]
    fn should_clear_dedup_on_overflow() {
        let bot = make_bot(vec![]);
        for i in 0..MAX_SEEN_IDS {
            bot.dedup_check(&format!("msg_{i}"));
        }
        assert!(!bot.dedup_check("new_msg"));
        assert!(!bot.dedup_check("msg_0"));
    }

    #[test]
    fn should_parse_text_callback() {
        let bot = make_bot(vec![]);
        let body: Value = json!({
            "msgid": "123456",
            "msgtype": "text",
            "chatid": "group_abc",
            "chattype": "group",
            "from": { "userid": "user123" },
            "text": { "content": "@bot hello world" }
        });

        let msg = bot.parse_callback(&body).unwrap();
        assert_eq!(msg.channel, "wecom-bot");
        assert_eq!(msg.sender_id, "user123");
        assert_eq!(msg.chat_id, "group_abc");
        assert_eq!(msg.content, "@bot hello world");
    }

    #[test]
    fn should_parse_voice_with_transcription() {
        let bot = make_bot(vec![]);
        let body: Value = json!({
            "msgid": "voice1",
            "msgtype": "voice",
            "chatid": "group_abc",
            "from": { "userid": "user1" },
            "voice": { "content": "transcribed text here" }
        });

        let msg = bot.parse_callback(&body).unwrap();
        assert_eq!(msg.content, "transcribed text here");
    }

    #[test]
    fn should_parse_mixed_message() {
        let bot = make_bot(vec![]);
        let body: Value = json!({
            "msgid": "mixed1",
            "msgtype": "mixed",
            "chatid": "group_abc",
            "from": { "userid": "user1" },
            "mixed": {
                "msg_item": [
                    { "msgtype": "text", "text": { "content": "look at this" } },
                    { "msgtype": "image", "image": { "url": "https://..." } }
                ]
            }
        });

        let msg = bot.parse_callback(&body).unwrap();
        assert_eq!(msg.content, "look at this [image]");
    }

    #[test]
    fn should_skip_event_callback() {
        let bot = make_bot(vec![]);
        let body: Value = json!({
            "msgtype": "event",
            "event": "enter_chat",
            "chatid": "group_abc",
            "from": { "userid": "user1" }
        });

        assert!(bot.parse_callback(&body).is_none());
    }

    #[test]
    fn should_filter_disallowed_sender() {
        let bot = make_bot(vec!["allowed_user"]);
        let body: Value = json!({
            "msgid": "789",
            "msgtype": "text",
            "chatid": "group_abc",
            "from": { "userid": "disallowed_user" },
            "text": { "content": "hello" }
        });

        assert!(bot.parse_callback(&body).is_none());
    }

    #[test]
    fn should_build_subscribe_frame() {
        let bot = make_bot(vec![]);
        let frame: Value = serde_json::from_str(&bot.subscribe_frame()).unwrap();
        assert_eq!(frame["cmd"], "aibot_subscribe");
        assert_eq!(frame["body"]["bot_id"], "test_bot_id");
        assert_eq!(frame["body"]["secret"], "test_secret");
    }

    #[test]
    fn should_build_send_msg_frame() {
        let frame: Value =
            serde_json::from_str(&WeComBotChannel::send_msg_frame("chat1", "hello **world**"))
                .unwrap();
        assert_eq!(frame["cmd"], "aibot_send_msg");
        assert_eq!(frame["body"]["chatid"], "chat1");
        assert_eq!(frame["body"]["msgtype"], "markdown");
        assert_eq!(frame["body"]["markdown"]["content"], "hello **world**");
    }

    #[test]
    fn should_build_ping_frame() {
        let frame: Value = serde_json::from_str(&WeComBotChannel::ping_frame()).unwrap();
        assert_eq!(frame["cmd"], "ping");
        assert!(
            frame["headers"]["req_id"]
                .as_str()
                .unwrap()
                .starts_with("ping_")
        );
    }
}
