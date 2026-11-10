//! WhatsApp channel via Node.js bridge (Baileys) over WebSocket.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use chrono::Utc;
use eyre::Result;
use futures::{SinkExt, StreamExt};
use octos_core::{InboundMessage, OutboundMessage};
use reqwest::Client;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{debug, error, info, warn};

use crate::channel::Channel;
use crate::media::download_media;

/// Default bridge URL for the Node.js WhatsApp bridge.
const DEFAULT_BRIDGE_URL: &str = "ws://localhost:3001";

/// Map a MIME type to a file extension.
fn ext_from_mimetype(mime: &str) -> String {
    match mime {
        "image/jpeg" => ".jpg",
        "image/png" => ".png",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        "audio/ogg" | "audio/ogg; codecs=opus" => ".ogg",
        "audio/mpeg" => ".mp3",
        "audio/mp4" => ".m4a",
        "video/mp4" => ".mp4",
        "application/pdf" => ".pdf",
        _ => "",
    }
    .to_string()
}

pub struct WhatsAppChannel {
    bridge_url: String,
    allowed_senders: HashSet<String>,
    shutdown: Arc<AtomicBool>,
    /// Write half of the WebSocket for sending messages.
    ws_tx: Arc<tokio::sync::Mutex<Option<WsSink>>>,
    media_dir: PathBuf,
    http: Client,
}

type WsSink = futures::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    WsMessage,
>;

impl WhatsAppChannel {
    pub fn new(
        bridge_url: &str,
        allowed_senders: Vec<String>,
        shutdown: Arc<AtomicBool>,
        media_dir: PathBuf,
    ) -> Self {
        let url = if bridge_url.is_empty() {
            DEFAULT_BRIDGE_URL.to_string()
        } else {
            bridge_url.to_string()
        };
        Self {
            bridge_url: url,
            allowed_senders: allowed_senders.into_iter().collect(),
            shutdown,
            ws_tx: Arc::new(tokio::sync::Mutex::new(None)),
            media_dir,
            http: crate::http_client::channel_http_client(),
        }
    }

    fn check_allowed(&self, sender_id: &str) -> bool {
        self.allowed_senders.is_empty() || self.allowed_senders.contains(sender_id)
    }

    /// Extract a clean sender ID from a WhatsApp JID (e.g. "1234567890@s.whatsapp.net" -> "1234567890").
    fn clean_sender(jid: &str) -> &str {
        jid.split('@').next().unwrap_or(jid)
    }
}

#[async_trait]
impl Channel for WhatsAppChannel {
    fn name(&self) -> &str {
        "whatsapp"
    }

    async fn start(&self, inbound_tx: mpsc::Sender<InboundMessage>) -> Result<()> {
        info!(url = %self.bridge_url, "Starting WhatsApp channel");

        let mut backoff_secs = 5u64;

        loop {
            if self.shutdown.load(Ordering::Relaxed) {
                break;
            }

            let (ws_stream, _) = match connect_async(&self.bridge_url).await {
                Ok(conn) => conn,
                Err(e) => {
                    error!("Failed to connect to WhatsApp bridge: {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(300); // Max 5 minutes
                    continue;
                }
            };

            backoff_secs = 5; // Reset on successful connection
            info!("WhatsApp bridge connected");
            let (ws_tx, mut ws_rx) = ws_stream.split();

            // Store write half for send()
            *self.ws_tx.lock().await = Some(ws_tx);

            while let Some(frame) = ws_rx.next().await {
                if self.shutdown.load(Ordering::Relaxed) {
                    break;
                }

                let data = match frame {
                    Ok(WsMessage::Text(text)) => text,
                    Ok(WsMessage::Close(_)) => {
                        info!("WhatsApp bridge closed connection");
                        break;
                    }
                    Ok(_) => continue,
                    Err(e) => {
                        warn!("WhatsApp WebSocket error: {e}");
                        break;
                    }
                };

                let msg: serde_json::Value = match serde_json::from_str(&data) {
                    Ok(v) => v,
                    Err(e) => {
                        debug!("Failed to parse WhatsApp bridge message: {e}");
                        continue;
                    }
                };

                let msg_type = msg.get("type").and_then(|v| v.as_str()).unwrap_or("");

                match msg_type {
                    "message" => {
                        let sender = msg.get("sender").and_then(|v| v.as_str()).unwrap_or("");
                        let content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
                        let is_group = msg
                            .get("isGroup")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);

                        // Download media attachments from bridge
                        let mut media = Vec::new();
                        if let Some(media_arr) = msg.get("media").and_then(|v| v.as_array()) {
                            for item in media_arr {
                                let url = item.get("url").and_then(|v| v.as_str()).unwrap_or("");
                                let mimetype =
                                    item.get("mimetype").and_then(|v| v.as_str()).unwrap_or("");
                                let filename =
                                    item.get("filename").and_then(|v| v.as_str()).unwrap_or("");

                                if url.is_empty() {
                                    continue;
                                }

                                let ext = if !filename.is_empty() {
                                    std::path::Path::new(filename)
                                        .extension()
                                        .map(|e| format!(".{}", e.to_string_lossy()))
                                        .unwrap_or_default()
                                } else {
                                    ext_from_mimetype(mimetype)
                                };
                                let dl_name =
                                    format!("wa_{}{}", Utc::now().timestamp_millis(), ext);

                                match download_media(
                                    &self.http,
                                    url,
                                    &[],
                                    &self.media_dir,
                                    &dl_name,
                                )
                                .await
                                {
                                    Ok(path) => media.push(path.display().to_string()),
                                    Err(e) => warn!("failed to download WhatsApp media: {e}"),
                                }
                            }
                        }
                        // Also handle single mediaUrl field
                        if media.is_empty() {
                            if let Some(url) = msg.get("mediaUrl").and_then(|v| v.as_str()) {
                                if !url.is_empty() {
                                    let mimetype =
                                        msg.get("mimetype").and_then(|v| v.as_str()).unwrap_or("");
                                    let ext = ext_from_mimetype(mimetype);
                                    let dl_name =
                                        format!("wa_{}{}", Utc::now().timestamp_millis(), ext);

                                    match download_media(
                                        &self.http,
                                        url,
                                        &[],
                                        &self.media_dir,
                                        &dl_name,
                                    )
                                    .await
                                    {
                                        Ok(path) => media.push(path.display().to_string()),
                                        Err(e) => warn!("failed to download WhatsApp media: {e}"),
                                    }
                                }
                            }
                        }

                        if sender.is_empty() || (content.is_empty() && media.is_empty()) {
                            continue;
                        }

                        let clean_id = Self::clean_sender(sender);

                        if !self.check_allowed(clean_id) {
                            continue;
                        }

                        // Use the full JID as chat_id so the bridge can send replies
                        // to the correct address (especially important for @lid JIDs).
                        let chat_id = if let Some(cid) = msg.get("chatId").and_then(|v| v.as_str())
                        {
                            cid.to_string()
                        } else {
                            sender.to_string()
                        };

                        let inbound = InboundMessage {
                            channel: "whatsapp".into(),
                            sender_id: clean_id.to_string(),
                            chat_id,
                            content: content.to_string(),
                            timestamp: Utc::now(),
                            media,
                            metadata: serde_json::json!({
                                "whatsapp": {
                                    "is_group": is_group,
                                    "jid": sender,
                                }
                            }),
                            message_id: None,
                            origin: octos_core::MessageOrigin::ExternalUser,
                        };

                        if inbound_tx.send(inbound).await.is_err() {
                            return Ok(());
                        }
                    }
                    "status" => {
                        let status = msg.get("status").and_then(|v| v.as_str()).unwrap_or("");
                        info!("WhatsApp status: {status}");
                    }
                    "qr" => {
                        info!("WhatsApp QR code received — scan with your phone");
                    }
                    "error" => {
                        let err = msg.get("error").and_then(|v| v.as_str()).unwrap_or("");
                        warn!("WhatsApp bridge error: {err}");
                    }
                    _ => {
                        debug!("WhatsApp unknown message type: {msg_type}");
                    }
                }
            }

            // Clear write half on disconnect
            *self.ws_tx.lock().await = None;

            if self.shutdown.load(Ordering::Relaxed) {
                break;
            }

            warn!("WhatsApp bridge disconnected, reconnecting in {backoff_secs}s...");
            tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
            backoff_secs = (backoff_secs * 2).min(300); // Max 5 minutes
        }

        info!("WhatsApp channel stopped");
        Ok(())
    }

    async fn send(&self, msg: &OutboundMessage) -> Result<()> {
        let mut lock = self.ws_tx.lock().await;
        let Some(ref mut tx) = *lock else {
            warn!("WhatsApp bridge not connected, cannot send");
            return Ok(());
        };

        if !msg.media.is_empty() {
            // Send each media file (bridge handles image/video/audio/document detection)
            for (i, path) in msg.media.iter().enumerate() {
                let caption = if i == 0 { &msg.content } else { "" };
                let payload = serde_json::json!({
                    "type": "send",
                    "to": msg.chat_id,
                    "text": caption,
                    "media": path,
                });
                tx.send(WsMessage::Text(payload.to_string().into()))
                    .await
                    .map_err(|e| eyre::eyre!("failed to send WhatsApp media: {e}"))?;
            }
        } else {
            let payload = serde_json::json!({
                "type": "send",
                "to": msg.chat_id,
                "text": msg.content,
            });
            tx.send(WsMessage::Text(payload.to_string().into()))
                .await
                .map_err(|e| eyre::eyre!("failed to send WhatsApp message: {e}"))?;
        }

        Ok(())
    }

    fn is_allowed(&self, sender_id: &str) -> bool {
        self.check_allowed(sender_id)
    }

    async fn stop(&self) -> Result<()> {
        self.shutdown.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn send_typing(&self, chat_id: &str) -> Result<()> {
        let mut lock = self.ws_tx.lock().await;
        let Some(ref mut tx) = *lock else {
            return Ok(());
        };

        let payload = serde_json::json!({
            "type": "typing",
            "to": chat_id,
        });
        tx.send(WsMessage::Text(payload.to_string().into()))
            .await
            .map_err(|e| eyre::eyre!("failed to send WhatsApp typing: {e}"))?;
        Ok(())
    }
}
