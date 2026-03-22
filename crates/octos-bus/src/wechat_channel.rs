//! WeChat channel — connects to wechat-bridge via WebSocket.
//!
//! The bridge maintains the persistent WeChat long-poll connection.
//! This channel just translates between the bridge's WS protocol and octos InboundMessage/OutboundMessage.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use eyre::{bail, Result};
use futures::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{debug, error, info, warn};

use octos_core::{InboundMessage, OutboundMessage};

use crate::channel::Channel;

type WsSink = futures::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    WsMessage,
>;

pub struct WeChatChannel {
    bridge_url: String,
    allowed_senders: HashSet<String>,
    shutdown: Arc<AtomicBool>,
    ws_tx: Arc<Mutex<Option<WsSink>>>,
}

impl WeChatChannel {
    pub fn new(
        bridge_url: &str,
        allowed_senders: Vec<String>,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        Self {
            bridge_url: bridge_url.to_string(),
            allowed_senders: allowed_senders.into_iter().collect(),
            shutdown,
            ws_tx: Arc::new(Mutex::new(None)),
        }
    }

    fn check_allowed(&self, sender_id: &str) -> bool {
        self.allowed_senders.is_empty() || self.allowed_senders.contains(sender_id)
    }

    async fn run_loop(&self, inbound_tx: &mpsc::Sender<InboundMessage>) -> Result<()> {
        loop {
            if self.shutdown.load(Ordering::Relaxed) {
                break;
            }

            info!(url = %self.bridge_url, "WeChat: connecting to bridge");

            match tokio_tungstenite::connect_async(&self.bridge_url).await {
                Ok((ws, _)) => {
                    info!("WeChat: connected to bridge");
                    let (ws_tx, mut ws_rx) = ws.split();
                    *self.ws_tx.lock().await = Some(ws_tx);

                    while let Some(frame) = ws_rx.next().await {
                        if self.shutdown.load(Ordering::Relaxed) {
                            break;
                        }
                        match frame {
                            Ok(WsMessage::Text(text)) => {
                                if let Ok(msg) = serde_json::from_str::<serde_json::Value>(&text) {
                                    if msg["type"].as_str() == Some("message") {
                                        if let Some(inbound) = self.parse_bridge_message(&msg) {
                                            if inbound_tx.send(inbound).await.is_err() {
                                                error!("WeChat: inbound_tx dropped");
                                                bail!("inbound channel closed");
                                            }
                                        }
                                    }
                                }
                            }
                            Ok(WsMessage::Close(_)) => {
                                warn!("WeChat: bridge connection closed");
                                break;
                            }
                            Err(e) => {
                                warn!(error = %e, "WeChat: bridge WS error");
                                break;
                            }
                            _ => {}
                        }
                    }

                    *self.ws_tx.lock().await = None;
                }
                Err(e) => {
                    warn!(error = %e, "WeChat: failed to connect to bridge");
                }
            }

            if self.shutdown.load(Ordering::Relaxed) {
                break;
            }

            warn!("WeChat: reconnecting to bridge in 3s...");
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }

        Ok(())
    }

    fn parse_bridge_message(&self, msg: &serde_json::Value) -> Option<InboundMessage> {
        let sender = msg["sender"].as_str().unwrap_or_default();
        let content = msg["content"].as_str().unwrap_or_default().trim();
        let context_token = msg["context_token"].as_str().unwrap_or_default();
        let message_id = msg["message_id"].as_str().map(|s| s.to_string());

        if content.is_empty() {
            return None;
        }

        if !self.check_allowed(sender) {
            debug!(sender, "WeChat: sender not allowed");
            return None;
        }

        info!(sender, "WeChat: received message via bridge");

        let metadata = json!({
            "wechat": {
                "context_token": context_token,
                "sender": sender,
            }
        });

        Some(InboundMessage {
            channel: "wechat".into(),
            sender_id: sender.to_string(),
            chat_id: sender.to_string(),
            content: content.to_string(),
            timestamp: Utc::now(),
            media: vec![],
            metadata,
            message_id,
        })
    }
}

#[async_trait]
impl Channel for WeChatChannel {
    fn name(&self) -> &str {
        "wechat"
    }

    async fn start(&self, inbound_tx: mpsc::Sender<InboundMessage>) -> Result<()> {
        info!("Starting WeChat channel (bridge mode)");
        self.run_loop(&inbound_tx).await?;
        info!("WeChat channel stopped");
        Ok(())
    }

    async fn send(&self, msg: &OutboundMessage) -> Result<()> {
        if msg.content.is_empty() {
            return Ok(());
        }

        let mut ws = self.ws_tx.lock().await;
        let tx = ws.as_mut().ok_or_else(|| eyre::eyre!("WeChat: not connected to bridge"))?;

        let payload = json!({
            "type": "send",
            "to": msg.chat_id,
            "text": msg.content,
        });

        let msg_text: String = payload.to_string();
        tx.send(WsMessage::Text(msg_text.into()))
            .await
            .map_err(|e| eyre::eyre!("WeChat: failed to send to bridge: {e}"))?;

        debug!(chat_id = %msg.chat_id, "WeChat: sent via bridge");
        Ok(())
    }

    fn is_allowed(&self, sender_id: &str) -> bool {
        self.check_allowed(sender_id)
    }

    fn max_message_length(&self) -> usize {
        4000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_channel() -> WeChatChannel {
        WeChatChannel {
            bridge_url: "ws://localhost:3201".into(),
            allowed_senders: HashSet::new(),
            shutdown: Arc::new(AtomicBool::new(false)),
            ws_tx: Arc::new(Mutex::new(None)),
        }
    }

    #[test]
    fn should_return_wechat_as_channel_name() {
        assert_eq!(make_channel().name(), "wechat");
    }

    #[test]
    fn should_parse_bridge_message() {
        let ch = make_channel();
        let msg = json!({"type": "message", "sender": "user@im.wechat", "content": "hello", "context_token": "ctx", "message_id": "123"});
        let inbound = ch.parse_bridge_message(&msg).unwrap();
        assert_eq!(inbound.channel, "wechat");
        assert_eq!(inbound.sender_id, "user@im.wechat");
        assert_eq!(inbound.content, "hello");
    }

    #[test]
    fn should_skip_empty_content() {
        let ch = make_channel();
        let msg = json!({"type": "message", "sender": "user@im.wechat", "content": "", "context_token": "ctx"});
        assert!(ch.parse_bridge_message(&msg).is_none());
    }

    #[test]
    fn should_return_max_message_length() {
        assert_eq!(make_channel().max_message_length(), 4000);
    }
}
