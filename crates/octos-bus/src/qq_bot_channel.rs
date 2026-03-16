//! QQ Bot channel — Official QQ Bot API v2 via WebSocket gateway.
//!
//! Connects to the QQ Bot Gateway via WebSocket using AppID + ClientSecret.
//! Receives group messages via `GROUP_AT_MESSAGE_CREATE` events (requires @mention).
//! Replies via REST API POST to `/v2/groups/{group_openid}/messages`.
//! No public callback URL required — the bot connects outbound via WebSocket.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use async_trait::async_trait;
use chrono::Utc;
use eyre::{Result, WrapErr, bail};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use octos_core::{InboundMessage, OutboundMessage};
use reqwest::Client;
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{debug, error, info, warn};

// Re-used for explicit TLS connector (avoids CryptoProvider auto-detection panic).
extern crate rustls;
extern crate rustls_native_certs;

use crate::channel::Channel;

/// QQ Bot token endpoint.
const TOKEN_URL: &str = "https://bots.qq.com/app/getAppAccessToken";
/// QQ Bot REST API base.
const API_BASE: &str = "https://api.sgroup.qq.com";
/// Max reconnection attempts.
const MAX_RECONNECT_ATTEMPTS: u32 = 100;
/// Base reconnect delay in milliseconds.
const RECONNECT_BASE_DELAY_MS: u64 = 5000;
/// Max reconnect delay in milliseconds.
const RECONNECT_MAX_DELAY_MS: u64 = 60000;
/// Maximum message IDs to track for dedup.
const MAX_SEEN_IDS: usize = 1000;
/// Max message length for QQ Bot text messages.
const MAX_MSG_LENGTH: usize = 4000;
/// Safety margin before token expiry (seconds).
const TOKEN_REFRESH_MARGIN_SECS: u64 = 60;
/// Intents: GROUP_AND_C2C_EVENT (1<<25) | PUBLIC_GUILD_MESSAGES (1<<30).
const INTENTS: u32 = (1 << 25) | (1 << 30);

type WsSink = SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    WsMessage,
>;

/// Cached access token with expiry.
struct TokenState {
    token: String,
    expires_at: Instant,
}

/// WebSocket session state for resume.
struct SessionState {
    session_id: String,
    seq: u64,
}

/// QQ Bot channel (Official API v2, WebSocket gateway).
///
/// - Authenticates via AppID + ClientSecret → access_token
/// - Discovers gateway URL, connects via WebSocket
/// - Receives group @mentions via `GROUP_AT_MESSAGE_CREATE`
/// - Replies via REST POST to QQ Bot API
/// - Heartbeat driven by server (OpCode 10 Hello)
/// - Auto-reconnects with exponential backoff, supports session resume
pub struct QQBotChannel {
    app_id: String,
    client_secret: String,
    allowed_senders: HashSet<String>,
    shutdown: Arc<AtomicBool>,
    seen_ids: Arc<std::sync::Mutex<HashSet<String>>>,
    http_client: Client,
    access_token: Arc<Mutex<Option<TokenState>>>,
    session_state: Arc<Mutex<Option<SessionState>>>,
    /// Monotonically increasing message sequence for replies.
    msg_seq: AtomicU64,
}

impl QQBotChannel {
    pub fn new(
        app_id: &str,
        client_secret: &str,
        allowed_senders: Vec<String>,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        Self {
            app_id: app_id.to_string(),
            client_secret: client_secret.to_string(),
            allowed_senders: allowed_senders.into_iter().collect(),
            shutdown,
            seen_ids: Arc::new(std::sync::Mutex::new(HashSet::new())),
            http_client: Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_else(|_| Client::new()),
            access_token: Arc::new(Mutex::new(None)),
            session_state: Arc::new(Mutex::new(None)),
            msg_seq: AtomicU64::new(1),
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

    /// Fetch or refresh the access token.
    async fn get_access_token(&self) -> Result<String> {
        {
            let guard = self.access_token.lock().await;
            if let Some(ref ts) = *guard {
                if ts.expires_at > Instant::now() {
                    return Ok(ts.token.clone());
                }
            }
        }

        // Refresh
        let resp = self
            .http_client
            .post(TOKEN_URL)
            .json(&json!({
                "appId": self.app_id,
                "clientSecret": self.client_secret,
            }))
            .send()
            .await
            .wrap_err("QQBot: failed to fetch access token")?;

        let status = resp.status();
        let body: Value = resp
            .json()
            .await
            .wrap_err("QQBot: failed to parse token response")?;

        if !status.is_success() {
            bail!(
                "QQBot: token request failed ({}): {}",
                status,
                body.to_string()
            );
        }

        let token = body["access_token"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("QQBot: missing access_token in response"))?
            .to_string();

        // expires_in is a string in QQ's API
        let expires_in: u64 = body["expires_in"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .or_else(|| body["expires_in"].as_u64())
            .unwrap_or(7200);

        let expires_at =
            Instant::now() + std::time::Duration::from_secs(expires_in - TOKEN_REFRESH_MARGIN_SECS);

        info!(expires_in, "QQBot: access token obtained");

        let mut guard = self.access_token.lock().await;
        *guard = Some(TokenState {
            token: token.clone(),
            expires_at,
        });

        Ok(token)
    }

    /// Fetch the WebSocket gateway URL.
    async fn fetch_gateway_url(&self, token: &str) -> Result<String> {
        let resp = self
            .http_client
            .get(format!("{API_BASE}/gateway"))
            .header("Authorization", format!("QQBotAccessToken {token}"))
            .send()
            .await
            .wrap_err("QQBot: failed to fetch gateway URL")?;

        let body: Value = resp
            .json()
            .await
            .wrap_err("QQBot: failed to parse gateway response")?;

        let url = body["url"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("QQBot: missing url in gateway response"))?
            .to_string();

        info!(url = %url, "QQBot: gateway URL obtained");
        Ok(url)
    }

    /// Send a reply to a group via REST API.
    async fn send_group_message(
        &self,
        group_openid: &str,
        content: &str,
        msg_id: Option<&str>,
    ) -> Result<()> {
        let token = self.get_access_token().await?;
        let seq = self.msg_seq.fetch_add(1, Ordering::Relaxed);

        let mut body = json!({
            "content": content,
            "msg_type": 0,
            "msg_seq": seq,
        });

        // If replying to a specific message, include msg_id for passive reply
        if let Some(mid) = msg_id {
            body["msg_id"] = json!(mid);
        }

        let resp = self
            .http_client
            .post(format!("{API_BASE}/v2/groups/{group_openid}/messages"))
            .header("Authorization", format!("QQBotAccessToken {token}"))
            .json(&body)
            .send()
            .await
            .wrap_err("QQBot: failed to send group message")?;

        let status = resp.status();
        if !status.is_success() {
            let err_body = resp.text().await.unwrap_or_default();
            warn!(
                status = %status,
                body = %err_body,
                group_openid,
                "QQBot: send message failed"
            );
            bail!("QQBot: send message failed ({status})");
        }

        debug!(group_openid, seq, "QQBot: message sent");
        Ok(())
    }

    /// Parse a GROUP_AT_MESSAGE_CREATE event into an InboundMessage.
    fn parse_group_message(&self, data: &Value) -> Option<InboundMessage> {
        let msg_id = data["id"].as_str().unwrap_or_default();
        let group_openid = data["group_openid"].as_str()?;
        let member_openid = data
            .get("author")
            .and_then(|a| a.get("member_openid"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let content = data["content"].as_str().unwrap_or_default().trim();

        if content.is_empty() {
            return None;
        }

        if !msg_id.is_empty() && self.dedup_check(msg_id) {
            debug!(msg_id, "QQBot: dedup filtered message");
            return None;
        }

        if !self.check_allowed(member_openid) {
            debug!(member_openid, "QQBot: sender not allowed");
            return None;
        }

        info!(
            msg_id,
            member_openid, group_openid, "QQBot: parsed group message"
        );

        let metadata = json!({
            "qq_bot": {
                "msg_id": msg_id,
                "group_openid": group_openid,
            }
        });

        Some(InboundMessage {
            channel: "qq-bot".into(),
            sender_id: member_openid.to_string(),
            chat_id: group_openid.to_string(),
            content: content.to_string(),
            timestamp: Utc::now(),
            media: vec![],
            metadata,
            message_id: Some(msg_id.to_string()),
        })
    }

    /// Build a TLS connector for the WebSocket connection.
    fn make_tls_connector() -> Result<tokio_tungstenite::Connector> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut root_store = rustls::RootCertStore::empty();
        for cert in rustls_native_certs::load_native_certs().certs {
            root_store.add(cert).ok();
        }
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .wrap_err("QQBot: failed to configure TLS")?
            .with_root_certificates(root_store)
            .with_no_client_auth();
        Ok(tokio_tungstenite::Connector::Rustls(Arc::new(config)))
    }

    /// Connect, identify, and process messages. Returns on disconnect/error.
    async fn run_connection(&self, inbound_tx: &mpsc::Sender<InboundMessage>) -> Result<()> {
        let token = self.get_access_token().await?;
        let gateway_url = self.fetch_gateway_url(&token).await?;

        info!(url = %gateway_url, "QQBot: connecting to gateway");

        let connector = Self::make_tls_connector()?;
        let (ws_stream, _) = tokio_tungstenite::connect_async_tls_with_config(
            &gateway_url,
            None,
            false,
            Some(connector),
        )
        .await
        .wrap_err("QQBot: failed to connect WebSocket")?;

        info!("QQBot: WebSocket connected");

        let (sink, stream) = ws_stream.split();
        let sink = Arc::new(Mutex::new(sink));

        // Process frames — Hello will trigger Identify
        let result = self.process_frames(stream, sink.clone(), inbound_tx).await;

        result
    }

    /// Process WebSocket frames (Hello, Dispatch, Heartbeat ACK, etc.).
    async fn process_frames(
        &self,
        mut stream: SplitStream<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
        >,
        sink: Arc<Mutex<WsSink>>,
        inbound_tx: &mpsc::Sender<InboundMessage>,
    ) -> Result<()> {
        let mut heartbeat_interval: Option<tokio::time::Interval> = None;
        let mut identified = false;

        loop {
            if self.shutdown.load(Ordering::Acquire) {
                info!("QQBot: shutdown requested");
                return Ok(());
            }

            tokio::select! {
                // Heartbeat tick
                _ = async {
                    match heartbeat_interval.as_mut() {
                        Some(interval) => interval.tick().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if !identified {
                        continue;
                    }
                    // Send heartbeat (OpCode 1)
                    let seq = {
                        let session = self.session_state.lock().await;
                        session.as_ref().map(|s| s.seq).unwrap_or(0)
                    };
                    let frame = json!({"op": 1, "d": seq});
                    let mut ws = sink.lock().await;
                    if let Err(e) = ws.send(WsMessage::Text(frame.to_string().into())).await {
                        warn!("QQBot: heartbeat send failed: {e}");
                        bail!("heartbeat send failed");
                    }
                    debug!("QQBot: heartbeat sent (seq={seq})");
                }
                // WebSocket message
                msg = stream.next() => {
                    match msg {
                        Some(Ok(WsMessage::Text(text))) => {
                            let text_str: &str = &text;
                            match serde_json::from_str::<Value>(text_str) {
                                Ok(frame) => {
                                    let op = frame["op"].as_u64().unwrap_or(99);
                                    match op {
                                        // OpCode 10: Hello — start heartbeat, send Identify
                                        10 => {
                                            let interval_ms = frame["d"]["heartbeat_interval"]
                                                .as_u64()
                                                .unwrap_or(41250);
                                            info!(interval_ms, "QQBot: Hello received");

                                            heartbeat_interval = Some(tokio::time::interval(
                                                std::time::Duration::from_millis(interval_ms),
                                            ));

                                            // Try resume first, otherwise identify
                                            let session = self.session_state.lock().await;
                                            if let Some(ref state) = *session {
                                                let token = self.get_access_token().await?;
                                                let resume = json!({
                                                    "op": 6,
                                                    "d": {
                                                        "token": format!("QQBotAccessToken {token}"),
                                                        "session_id": state.session_id,
                                                        "seq": state.seq,
                                                    }
                                                });
                                                drop(session);
                                                let mut ws = sink.lock().await;
                                                ws.send(WsMessage::Text(resume.to_string().into()))
                                                    .await
                                                    .wrap_err("QQBot: failed to send Resume")?;
                                                info!("QQBot: Resume sent");
                                            } else {
                                                drop(session);
                                                let token = self.get_access_token().await?;
                                                let identify = json!({
                                                    "op": 2,
                                                    "d": {
                                                        "token": format!("QQBotAccessToken {token}"),
                                                        "intents": INTENTS,
                                                        "shard": [0, 1],
                                                    }
                                                });
                                                let mut ws = sink.lock().await;
                                                ws.send(WsMessage::Text(identify.to_string().into()))
                                                    .await
                                                    .wrap_err("QQBot: failed to send Identify")?;
                                                info!("QQBot: Identify sent");
                                            }
                                        }
                                        // OpCode 11: Heartbeat ACK
                                        11 => {
                                            debug!("QQBot: heartbeat ACK");
                                        }
                                        // OpCode 0: Dispatch (event)
                                        0 => {
                                            let event_type = frame["t"].as_str().unwrap_or("");
                                            let seq = frame["s"].as_u64().unwrap_or(0);

                                            // Update session sequence
                                            if seq > 0 {
                                                let mut session = self.session_state.lock().await;
                                                if let Some(ref mut state) = *session {
                                                    state.seq = seq;
                                                }
                                            }

                                            match event_type {
                                                "READY" => {
                                                    // Extract session_id for resume
                                                    let session_id = frame["d"]["session_id"]
                                                        .as_str()
                                                        .unwrap_or_default()
                                                        .to_string();
                                                    info!(session_id = %session_id, "QQBot: READY");
                                                    identified = true;

                                                    let mut session = self.session_state.lock().await;
                                                    *session = Some(SessionState {
                                                        session_id,
                                                        seq,
                                                    });
                                                }
                                                "RESUMED" => {
                                                    info!("QQBot: session resumed");
                                                    identified = true;
                                                }
                                                "GROUP_AT_MESSAGE_CREATE" => {
                                                    if let Some(data) = frame.get("d") {
                                                        if let Some(inbound) = self.parse_group_message(data) {
                                                            if inbound_tx.send(inbound).await.is_err() {
                                                                error!("QQBot: inbound_tx dropped");
                                                                bail!("inbound channel closed");
                                                            }
                                                        }
                                                    }
                                                }
                                                "C2C_MESSAGE_CREATE" => {
                                                    // Private message — could be handled similarly
                                                    debug!(event_type, "QQBot: C2C message (not yet supported)");
                                                }
                                                other => {
                                                    debug!(event = other, "QQBot: unhandled event");
                                                }
                                            }
                                        }
                                        // OpCode 7: Reconnect
                                        7 => {
                                            warn!("QQBot: server requested reconnect");
                                            bail!("server requested reconnect");
                                        }
                                        // OpCode 9: Invalid Session
                                        9 => {
                                            let resumable = frame["d"].as_bool().unwrap_or(false);
                                            if !resumable {
                                                warn!("QQBot: invalid session (not resumable), clearing state");
                                                let mut session = self.session_state.lock().await;
                                                *session = None;
                                            }
                                            bail!("invalid session (resumable={resumable})");
                                        }
                                        other => {
                                            debug!(op = other, "QQBot: unknown opcode");
                                        }
                                    }
                                }
                                Err(e) => {
                                    warn!("QQBot: malformed JSON frame: {e}");
                                }
                            }
                        }
                        Some(Ok(WsMessage::Ping(data))) => {
                            let mut ws = sink.lock().await;
                            let _ = ws.send(WsMessage::Pong(data)).await;
                        }
                        Some(Ok(WsMessage::Close(_))) => {
                            info!("QQBot: server closed connection");
                            bail!("server closed connection");
                        }
                        Some(Err(e)) => {
                            warn!("QQBot: WebSocket error: {e}");
                            bail!("WebSocket error: {e}");
                        }
                        None => {
                            info!("QQBot: WebSocket stream ended");
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

            match self.run_connection(&inbound_tx).await {
                Ok(()) => {
                    break;
                }
                Err(e) => {
                    attempts += 1;
                    if attempts > MAX_RECONNECT_ATTEMPTS {
                        error!("QQBot: max reconnect attempts reached, giving up");
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
                        "QQBot: connection lost, reconnecting"
                    );

                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
            }
        }
    }
}

#[async_trait]
impl Channel for QQBotChannel {
    fn name(&self) -> &str {
        "qq-bot"
    }

    async fn start(&self, inbound_tx: mpsc::Sender<InboundMessage>) -> Result<()> {
        info!("Starting QQBot channel (Official API v2, WebSocket gateway)");
        self.run_loop(inbound_tx).await;
        info!("QQBot channel stopped");
        Ok(())
    }

    async fn send(&self, msg: &OutboundMessage) -> Result<()> {
        if msg.content.is_empty() {
            return Ok(());
        }

        info!(
            chat_id = %msg.chat_id,
            content_len = msg.content.len(),
            "QQBot: sending message"
        );

        // Extract msg_id from metadata for passive reply (within 5min window)
        let msg_id = msg
            .metadata
            .get("qq_bot")
            .and_then(|q| q.get("msg_id"))
            .and_then(|v| v.as_str());

        self.send_group_message(&msg.chat_id, &msg.content, msg_id)
            .await?;

        if !msg.media.is_empty() {
            warn!(
                count = msg.media.len(),
                "QQBot: file attachments not yet supported, skipping"
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
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_bot(allowed: Vec<&str>) -> QQBotChannel {
        QQBotChannel {
            app_id: "test_app_id".into(),
            client_secret: "test_secret".into(),
            allowed_senders: allowed.into_iter().map(String::from).collect(),
            shutdown: Arc::new(AtomicBool::new(false)),
            seen_ids: Arc::new(std::sync::Mutex::new(HashSet::new())),
            http_client: Client::new(),
            access_token: Arc::new(Mutex::new(None)),
            session_state: Arc::new(Mutex::new(None)),
            msg_seq: AtomicU64::new(1),
        }
    }

    #[test]
    fn should_return_qq_bot_as_channel_name() {
        let bot = make_bot(vec![]);
        assert_eq!(bot.name(), "qq-bot");
    }

    #[test]
    fn should_return_max_message_length() {
        let bot = make_bot(vec![]);
        assert_eq!(bot.max_message_length(), MAX_MSG_LENGTH);
    }

    #[test]
    fn should_allow_anyone_when_empty_list() {
        let bot = make_bot(vec![]);
        assert!(bot.is_allowed("anyone"));
    }

    #[test]
    fn should_deny_unlisted_sender() {
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
    fn should_parse_group_at_message() {
        let bot = make_bot(vec![]);
        let data: Value = json!({
            "id": "msg_123",
            "group_openid": "group_abc",
            "author": {
                "member_openid": "member_xyz"
            },
            "content": "How do I use the API?"
        });

        let msg = bot.parse_group_message(&data).unwrap();
        assert_eq!(msg.channel, "qq-bot");
        assert_eq!(msg.sender_id, "member_xyz");
        assert_eq!(msg.chat_id, "group_abc");
        assert_eq!(msg.content, "How do I use the API?");
        assert_eq!(msg.metadata["qq_bot"]["msg_id"], "msg_123");
        assert_eq!(msg.metadata["qq_bot"]["group_openid"], "group_abc");
    }

    #[test]
    fn should_skip_empty_content() {
        let bot = make_bot(vec![]);
        let data: Value = json!({
            "id": "msg_456",
            "group_openid": "group_abc",
            "author": { "member_openid": "user1" },
            "content": "   "
        });

        assert!(bot.parse_group_message(&data).is_none());
    }

    #[test]
    fn should_filter_disallowed_sender() {
        let bot = make_bot(vec!["allowed_user"]);
        let data: Value = json!({
            "id": "msg_789",
            "group_openid": "group_abc",
            "author": { "member_openid": "disallowed_user" },
            "content": "hello"
        });

        assert!(bot.parse_group_message(&data).is_none());
    }

    #[test]
    fn should_dedup_parsed_messages() {
        let bot = make_bot(vec![]);
        let data: Value = json!({
            "id": "msg_dup",
            "group_openid": "group_abc",
            "author": { "member_openid": "user1" },
            "content": "hello"
        });

        assert!(bot.parse_group_message(&data).is_some());
        assert!(bot.parse_group_message(&data).is_none());
    }

    #[test]
    fn should_skip_missing_group_openid() {
        let bot = make_bot(vec![]);
        let data: Value = json!({
            "id": "msg_no_group",
            "author": { "member_openid": "user1" },
            "content": "hello"
        });

        assert!(bot.parse_group_message(&data).is_none());
    }
}
