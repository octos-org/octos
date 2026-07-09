//! DingTalk channel with custom-robot send and outgoing-robot webhook receive.
//!
//! Outbound sends use a configured custom robot webhook when available. Inbound
//! outgoing-robot events can also carry a short-lived `sessionWebhook`; the
//! channel caches that per conversation so replies go back to the originating
//! DingTalk chat.

use std::{
    collections::{HashMap, HashSet},
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
use reqwest::{Client, Url};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::channel::Channel;
use crate::coalesce::{ChunkConfig, split_message};
use crate::dedup::MessageDedup;

const DEFAULT_DINGTALK_WEBHOOK_PORT: u16 = 8650;

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

fn dingtalk_signature(timestamp: &str, secret: &str) -> String {
    let string_to_sign = format!("{timestamp}\n{secret}");
    BASE64.encode(hmac_sha256(secret.as_bytes(), string_to_sign.as_bytes()))
}

fn verify_dingtalk_signature(secret: &str, timestamp: &str, signature: &str) -> bool {
    if timestamp.is_empty() || signature.is_empty() {
        return false;
    }
    let computed = dingtalk_signature(timestamp, secret);
    computed.as_bytes().ct_eq(signature.as_bytes()).into()
}

fn signed_webhook_url(base_url: &str, timestamp: &str, secret: &str) -> Result<String> {
    let mut url = Url::parse(base_url).wrap_err("invalid DingTalk webhook URL")?;
    let sign = dingtalk_signature(timestamp, secret);
    url.query_pairs_mut()
        .append_pair("timestamp", timestamp)
        .append_pair("sign", &sign);
    Ok(url.to_string())
}

#[derive(Clone)]
struct WebhookState {
    sign_secret: Option<String>,
    inbound_tx: mpsc::Sender<serde_json::Value>,
}

async fn handle_webhook(
    State(state): State<WebhookState>,
    headers: HeaderMap,
    body: String,
) -> axum::response::Response {
    if let Some(ref secret) = state.sign_secret {
        let timestamp = headers
            .get("timestamp")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let signature = headers
            .get("sign")
            .or_else(|| headers.get("signature"))
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        if !verify_dingtalk_signature(secret, timestamp, signature) {
            warn!("DingTalk webhook: signature mismatch");
            return (
                axum::http::StatusCode::UNAUTHORIZED,
                axum::Json(serde_json::json!({"error": "signature mismatch"})),
            )
                .into_response();
        }
    }

    let payload: serde_json::Value = match serde_json::from_str(&body) {
        Ok(value) => value,
        Err(error) => {
            warn!("DingTalk webhook: invalid JSON body: {error}");
            return (
                axum::http::StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({"error": "invalid json"})),
            )
                .into_response();
        }
    };

    let _ = state.inbound_tx.send(payload).await;
    axum::Json(serde_json::json!({"msgtype": "text", "text": {"content": "ok"}})).into_response()
}

pub struct DingTalkChannel {
    webhook_url: Option<String>,
    sign_secret: Option<String>,
    allowed_senders: HashSet<String>,
    shutdown: Arc<AtomicBool>,
    http: Client,
    webhook_port: u16,
    dedup: MessageDedup,
    session_webhooks: Arc<Mutex<HashMap<String, String>>>,
}

impl DingTalkChannel {
    pub fn new(
        webhook_url: Option<String>,
        sign_secret: Option<String>,
        allowed_senders: Vec<String>,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        Self {
            webhook_url,
            sign_secret,
            allowed_senders: allowed_senders.into_iter().collect(),
            shutdown,
            http: Client::new(),
            webhook_port: DEFAULT_DINGTALK_WEBHOOK_PORT,
            dedup: MessageDedup::new(),
            session_webhooks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn with_webhook_port(mut self, port: u16) -> Self {
        self.webhook_port = port;
        self
    }

    fn check_allowed(&self, sender_id: &str) -> bool {
        self.allowed_senders.is_empty() || self.allowed_senders.contains(sender_id)
    }

    fn target_webhook(&self, msg: &OutboundMessage) -> Result<String> {
        if let Some(url) = msg
            .metadata
            .get("dingtalk")
            .and_then(|v| v.get("session_webhook"))
            .and_then(|v| v.as_str())
            .filter(|value| !value.trim().is_empty())
        {
            return Ok(url.to_string());
        }

        if msg.chat_id.starts_with("https://") || msg.chat_id.starts_with("http://") {
            return Ok(msg.chat_id.clone());
        }

        if let Some(url) = self
            .session_webhooks
            .lock()
            .expect("DingTalk session webhook lock")
            .get(&msg.chat_id)
            .cloned()
        {
            return Ok(url);
        }

        let Some(base_url) = self.webhook_url.as_deref() else {
            return Err(eyre::eyre!(
                "DingTalk send target unavailable: no sessionWebhook cached for chat '{}' and no configured webhook URL",
                msg.chat_id
            ));
        };

        if let Some(secret) = self.sign_secret.as_deref() {
            signed_webhook_url(base_url, &Utc::now().timestamp_millis().to_string(), secret)
        } else {
            Ok(base_url.to_string())
        }
    }

    fn build_text_payload(content: &str) -> serde_json::Value {
        serde_json::json!({
            "msgtype": "text",
            "text": {
                "content": content,
            },
        })
    }

    fn text_from_payload(payload: &serde_json::Value) -> String {
        if let Some(content) = payload
            .get("text")
            .and_then(|v| v.get("content"))
            .and_then(|v| v.as_str())
        {
            return content.trim().to_string();
        }

        if let Some(items) = payload
            .get("content")
            .and_then(|v| v.get("richText"))
            .and_then(|v| v.as_array())
        {
            let parts: Vec<&str> = items
                .iter()
                .filter_map(|item| item.get("text").and_then(|v| v.as_str()))
                .collect();
            let text = parts.join("");
            if !text.trim().is_empty() {
                return text.trim().to_string();
            }
        }

        payload
            .get("content")
            .and_then(|v| v.get("recognition"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string()
    }

    fn parse_event(&self, payload: &serde_json::Value) -> Option<InboundMessage> {
        let message_id = payload
            .get("msgId")
            .or_else(|| payload.get("msgid"))
            .and_then(|v| v.as_str())?;
        if message_id.is_empty() || self.dedup.is_duplicate(message_id) {
            debug!(message_id, "DingTalk: dedup filtered message");
            return None;
        }

        let sender_id = payload
            .get("senderStaffId")
            .or_else(|| payload.get("senderId"))
            .or_else(|| payload.get("senderUnionId"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if sender_id.is_empty() || !self.check_allowed(&sender_id) {
            return None;
        }

        let conversation_id = payload
            .get("conversationId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if conversation_id.is_empty() {
            return None;
        }

        if let Some(session_webhook) = payload
            .get("sessionWebhook")
            .and_then(|v| v.as_str())
            .filter(|value| !value.trim().is_empty())
        {
            self.session_webhooks
                .lock()
                .expect("DingTalk session webhook lock")
                .insert(conversation_id.clone(), session_webhook.to_string());
        }

        let msg_type = payload
            .get("msgtype")
            .or_else(|| payload.get("msgType"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let mut content = Self::text_from_payload(payload);
        if content.is_empty() {
            content = format!("[{msg_type} message]");
        }

        Some(InboundMessage {
            channel: "dingtalk".into(),
            sender_id,
            chat_id: conversation_id,
            content,
            timestamp: Utc::now(),
            media: vec![],
            metadata: serde_json::json!({
                "dingtalk": {
                    "message_type": msg_type,
                    "conversation_type": payload
                        .get("conversationType")
                        .and_then(|v| v.as_str()),
                    "conversation_title": payload
                        .get("conversationTitle")
                        .and_then(|v| v.as_str()),
                    "sender_nick": payload
                        .get("senderNick")
                        .and_then(|v| v.as_str()),
                    "session_webhook": payload
                        .get("sessionWebhook")
                        .and_then(|v| v.as_str()),
                }
            }),
            message_id: Some(message_id.to_string()),
            origin: octos_core::MessageOrigin::ExternalUser,
        })
    }

    async fn start_webhook(&self, inbound_tx: mpsc::Sender<InboundMessage>) -> Result<()> {
        let (event_tx, mut event_rx) = mpsc::channel::<serde_json::Value>(100);
        let app = Router::new()
            .route("/dingtalk/webhook", post(handle_webhook))
            .with_state(WebhookState {
                sign_secret: self.sign_secret.clone(),
                inbound_tx: event_tx,
            });

        let addr = format!("0.0.0.0:{}", self.webhook_port);
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .wrap_err_with(|| format!("failed to bind DingTalk webhook server to {addr}"))?;
        info!(
            port = self.webhook_port,
            "DingTalk webhook server listening"
        );

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

            if let Some(inbound) = self.parse_event(&payload) {
                info!(
                    sender = %inbound.sender_id,
                    chat = %inbound.chat_id,
                    "DingTalk: sending to inbound bus"
                );
                if inbound_tx.send(inbound).await.is_err() {
                    error!("DingTalk: inbound_tx send failed (receiver dropped)");
                    return Ok(());
                }
            }
        }

        Ok(())
    }
}

#[async_trait]
impl Channel for DingTalkChannel {
    fn name(&self) -> &str {
        "dingtalk"
    }

    async fn start(&self, inbound_tx: mpsc::Sender<InboundMessage>) -> Result<()> {
        info!(port = self.webhook_port, "Starting DingTalk channel");
        self.start_webhook(inbound_tx).await?;
        info!("DingTalk channel stopped");
        Ok(())
    }

    async fn send(&self, msg: &OutboundMessage) -> Result<()> {
        let target = self.target_webhook(msg)?;
        let config = ChunkConfig { max_chars: 3600 };
        for chunk in split_message(&msg.content, &config) {
            let resp = self
                .http
                .post(&target)
                .header("Content-Type", "application/json")
                .json(&Self::build_text_payload(&chunk))
                .send()
                .await
                .wrap_err("DingTalk send request failed")?;

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                warn!("DingTalk send error (HTTP {status}): {body}");
            }
        }

        if !msg.media.is_empty() {
            warn!(
                media_count = msg.media.len(),
                "DingTalk: outbound media is not supported by the text robot path"
            );
        }

        Ok(())
    }

    fn is_allowed(&self, sender_id: &str) -> bool {
        self.check_allowed(sender_id)
    }

    fn max_message_length(&self) -> usize {
        3600
    }

    async fn stop(&self) -> Result<()> {
        self.shutdown.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_channel(allowed: Vec<&str>) -> DingTalkChannel {
        DingTalkChannel::new(
            Some("https://oapi.dingtalk.com/robot/send?access_token=abc".into()),
            Some("SEC000".into()),
            allowed.into_iter().map(String::from).collect(),
            Arc::new(AtomicBool::new(false)),
        )
    }

    #[test]
    fn verifies_dingtalk_signature() {
        let timestamp = "1710000000000";
        let secret = "SEC000";
        let signature = dingtalk_signature(timestamp, secret);
        assert!(verify_dingtalk_signature(secret, timestamp, &signature));
        assert!(!verify_dingtalk_signature(secret, timestamp, "invalid"));
    }

    #[test]
    fn signed_webhook_url_adds_timestamp_and_sign() {
        let url = signed_webhook_url(
            "https://oapi.dingtalk.com/robot/send?access_token=abc",
            "1710000000000",
            "SEC000",
        )
        .unwrap();

        assert!(url.contains("access_token=abc"));
        assert!(url.contains("timestamp=1710000000000"));
        assert!(url.contains("sign="));
    }

    #[test]
    fn parse_text_event_caches_session_webhook() {
        let channel = make_channel(vec!["staff-1"]);
        let payload = serde_json::json!({
            "msgId": "msg-1",
            "msgtype": "text",
            "conversationId": "cid-1",
            "conversationType": "2",
            "conversationTitle": "Ops",
            "senderStaffId": "staff-1",
            "senderNick": "Yang",
            "sessionWebhook": "https://oapi.dingtalk.com/robot/sendBySession?session=xyz",
            "text": {"content": "hello octos"}
        });

        let inbound = channel.parse_event(&payload).unwrap();
        assert_eq!(inbound.channel, "dingtalk");
        assert_eq!(inbound.chat_id, "cid-1");
        assert_eq!(inbound.sender_id, "staff-1");
        assert_eq!(inbound.content, "hello octos");
        assert_eq!(inbound.message_id.as_deref(), Some("msg-1"));
        assert_eq!(
            channel
                .session_webhooks
                .lock()
                .unwrap()
                .get("cid-1")
                .map(String::as_str),
            Some("https://oapi.dingtalk.com/robot/sendBySession?session=xyz")
        );
    }

    #[test]
    fn parse_event_respects_allowed_senders() {
        let channel = make_channel(vec!["staff-allowed"]);
        let payload = serde_json::json!({
            "msgId": "msg-2",
            "msgtype": "text",
            "conversationId": "cid-1",
            "senderStaffId": "staff-denied",
            "text": {"content": "blocked"}
        });

        assert!(channel.parse_event(&payload).is_none());
    }

    #[test]
    fn target_webhook_prefers_cached_session_webhook() {
        let channel = make_channel(vec![]);
        channel
            .session_webhooks
            .lock()
            .unwrap()
            .insert("cid-1".into(), "https://session.example/send".into());
        let outbound = OutboundMessage {
            channel: "dingtalk".into(),
            chat_id: "cid-1".into(),
            content: "reply".into(),
            reply_to: None,
            media: vec![],
            metadata: serde_json::json!({}),
        };

        assert_eq!(
            channel.target_webhook(&outbound).unwrap(),
            "https://session.example/send"
        );
    }
}
