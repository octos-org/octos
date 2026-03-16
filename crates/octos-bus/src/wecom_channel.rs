//! WeCom (企业微信/WeChat Work) channel with webhook callback mode + REST API.
//!
//! Uses a Custom App with message callback URL for receiving messages,
//! and the WeCom REST API for sending messages.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use async_trait::async_trait;
use chrono::Utc;
use octos_core::{InboundMessage, OutboundMessage};
use eyre::{Result, WrapErr};
use reqwest::Client;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::channel::Channel;
use crate::media::{download_media, is_image};
use crate::wecom_crypto::{
    decode_aes_key, decrypt_wecom_message, verify_wecom_signature, xml_extract,
};

/// Token refresh interval (slightly under 2 hours).
const TOKEN_TTL_SECS: u64 = 7000;
/// Maximum message IDs to track for dedup.
const MAX_SEEN_IDS: usize = 1000;
/// WeCom API base URL.
const WECOM_API: &str = "https://qyapi.weixin.qq.com/cgi-bin";

// ---------------------------------------------------------------------------
// WeComChannel
// ---------------------------------------------------------------------------

pub struct WeComChannel {
    corp_id: String,
    agent_id: String,
    agent_secret: String,
    verification_token: String,
    aes_key: [u8; 32],
    allowed_senders: HashSet<String>,
    shutdown: Arc<AtomicBool>,
    http: Client,
    media_dir: PathBuf,
    token_cache: Arc<tokio::sync::Mutex<Option<(String, Instant)>>>,
    seen_ids: Arc<std::sync::Mutex<HashSet<String>>>,
    webhook_port: u16,
}

impl WeComChannel {
    pub fn new(
        corp_id: &str,
        agent_id: &str,
        agent_secret: &str,
        verification_token: &str,
        encoding_aes_key: &str,
        allowed_senders: Vec<String>,
        shutdown: Arc<AtomicBool>,
        media_dir: PathBuf,
    ) -> Self {
        let aes_key = decode_aes_key(encoding_aes_key).unwrap_or_else(|e| {
            warn!("WeCom: failed to decode EncodingAESKey: {e}, using zero key");
            [0u8; 32]
        });
        Self {
            corp_id: corp_id.to_string(),
            agent_id: agent_id.to_string(),
            agent_secret: agent_secret.to_string(),
            verification_token: verification_token.to_string(),
            aes_key,
            allowed_senders: allowed_senders.into_iter().collect(),
            shutdown,
            http: Client::new(),
            media_dir,
            token_cache: Arc::new(tokio::sync::Mutex::new(None)),
            seen_ids: Arc::new(std::sync::Mutex::new(HashSet::new())),
            webhook_port: 9322,
        }
    }

    /// Set webhook port (default 9322).
    pub fn with_webhook_port(mut self, port: u16) -> Self {
        self.webhook_port = port;
        self
    }

    fn check_allowed(&self, sender_id: &str) -> bool {
        self.allowed_senders.is_empty() || self.allowed_senders.contains(sender_id)
    }

    /// Get or refresh access token.
    async fn get_token(&self) -> Result<String> {
        let mut cache = self.token_cache.lock().await;
        if let Some((ref token, ref created)) = *cache {
            if created.elapsed().as_secs() < TOKEN_TTL_SECS {
                return Ok(token.clone());
            }
        }

        let url = format!(
            "{WECOM_API}/gettoken?corpid={}&corpsecret={}",
            self.corp_id, self.agent_secret
        );
        let resp: serde_json::Value = self
            .http
            .get(&url)
            .send()
            .await
            .wrap_err("failed to get WeCom access token")?
            .json()
            .await?;

        let errcode = resp.get("errcode").and_then(|v| v.as_i64()).unwrap_or(-1);
        if errcode != 0 {
            let errmsg = resp
                .get("errmsg")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            return Err(eyre::eyre!("WeCom token error: {errmsg} (code {errcode})"));
        }

        let token = resp
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| eyre::eyre!("WeCom: no access_token in response"))?
            .to_string();

        *cache = Some((token.clone(), Instant::now()));
        Ok(token)
    }

    /// Download a media resource from WeCom.
    async fn download_media_file(&self, media_id: &str, ext: &str) -> Result<PathBuf> {
        let token = self.get_token().await?;
        let url = format!("{WECOM_API}/media/get?access_token={token}&media_id={media_id}");
        let filename = format!("wecom_{}{}", Utc::now().timestamp_millis(), ext);
        download_media(&self.http, &url, &[], &self.media_dir, &filename).await
    }

    /// Upload a media file and return the media_id.
    async fn upload_media(&self, file_path: &str, media_type: &str) -> Result<String> {
        let token = self.get_token().await?;
        let data = std::fs::read(file_path).wrap_err("failed to read media file")?;
        let filename = std::path::Path::new(file_path)
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| "file".to_string());

        let part = reqwest::multipart::Part::bytes(data)
            .file_name(filename)
            .mime_str("application/octet-stream")?;
        let form = reqwest::multipart::Form::new().part("media", part);

        let url = format!("{WECOM_API}/media/upload?access_token={token}&type={media_type}");
        let resp: serde_json::Value = self
            .http
            .post(&url)
            .multipart(form)
            .send()
            .await
            .wrap_err("failed to upload media to WeCom")?
            .json()
            .await?;

        resp.get("media_id")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| {
                let errmsg = resp
                    .get("errmsg")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                eyre::eyre!("WeCom media upload error: {errmsg}")
            })
    }

    /// Send a message via WeCom REST API.
    async fn send_wecom_message(&self, user_id: &str, body: &serde_json::Value) -> Result<()> {
        let token = self.get_token().await?;
        let url = format!("{WECOM_API}/message/send?access_token={token}");

        let mut payload = body.clone();
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("touser".into(), serde_json::json!(user_id));
            obj.insert(
                "agentid".into(),
                serde_json::json!(self.agent_id.parse::<i64>().unwrap_or(0)),
            );
        }

        let resp: serde_json::Value = self
            .http
            .post(&url)
            .json(&payload)
            .send()
            .await
            .wrap_err("failed to send WeCom message")?
            .json()
            .await?;

        let errcode = resp.get("errcode").and_then(|v| v.as_i64()).unwrap_or(-1);
        if errcode != 0 {
            let errmsg = resp
                .get("errmsg")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            warn!("WeCom send error: {errmsg} (code {errcode})");
        }

        Ok(())
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

    /// Parse a decrypted WeCom XML message into an InboundMessage.
    async fn parse_message(&self, xml: &str) -> Option<InboundMessage> {
        let msg_type = xml_extract(xml, "MsgType")?;
        let from_user = xml_extract(xml, "FromUserName")?;
        let msg_id = xml_extract(xml, "MsgId").unwrap_or_default();

        if !msg_id.is_empty() && self.dedup_check(&msg_id) {
            debug!(msg_id, "WeCom: dedup filtered message");
            return None;
        }

        if !self.check_allowed(&from_user) {
            debug!(from_user, "WeCom: sender not allowed");
            return None;
        }

        let mut content = String::new();
        let mut media = Vec::new();

        match msg_type.as_str() {
            "text" => {
                content = xml_extract(xml, "Content").unwrap_or_default();
            }
            "image" => {
                if let Some(media_id) = xml_extract(xml, "MediaId") {
                    match self.download_media_file(&media_id, ".png").await {
                        Ok(path) => media.push(path.display().to_string()),
                        Err(e) => warn!("failed to download WeCom image: {e}"),
                    }
                }
            }
            "voice" => {
                if let Some(media_id) = xml_extract(xml, "MediaId") {
                    match self.download_media_file(&media_id, ".amr").await {
                        Ok(path) => media.push(path.display().to_string()),
                        Err(e) => warn!("failed to download WeCom voice: {e}"),
                    }
                }
            }
            "video" => {
                if let Some(media_id) = xml_extract(xml, "MediaId") {
                    match self.download_media_file(&media_id, ".mp4").await {
                        Ok(path) => media.push(path.display().to_string()),
                        Err(e) => warn!("failed to download WeCom video: {e}"),
                    }
                }
            }
            "location" => {
                let label = xml_extract(xml, "Label").unwrap_or_default();
                content = format!("[location: {label}]");
            }
            _ => {
                content = format!("[{msg_type} message]");
            }
        }

        if content.is_empty() && media.is_empty() {
            debug!(msg_type, "WeCom: empty content and media, skipping");
            return None;
        }

        info!(
            msg_id,
            msg_type,
            from_user,
            media_count = media.len(),
            "WeCom: parsed message"
        );

        Some(InboundMessage {
            channel: "wecom".into(),
            sender_id: from_user,
            chat_id: xml_extract(xml, "FromUserName").unwrap_or_default(),
            content,
            timestamp: Utc::now(),
            media,
            metadata: serde_json::json!({
                "wecom": {
                    "msg_id": msg_id,
                    "msg_type": msg_type,
                    "agent_id": xml_extract(xml, "AgentID").unwrap_or_default(),
                }
            }),
            message_id: None,
        })
    }

    /// Run webhook HTTP server.
    async fn start_webhook(&self, inbound_tx: mpsc::Sender<InboundMessage>) -> Result<()> {
        use axum::{
            Router,
            extract::{Query, State},
            response::IntoResponse,
            routing::get,
        };

        #[derive(Clone)]
        struct WebhookState {
            verification_token: String,
            aes_key: [u8; 32],
            corp_id: String,
            inbound_tx: mpsc::Sender<String>,
        }

        #[derive(serde::Deserialize)]
        struct VerifyParams {
            msg_signature: String,
            timestamp: String,
            nonce: String,
            echostr: String,
        }

        #[derive(serde::Deserialize)]
        struct CallbackParams {
            msg_signature: String,
            timestamp: String,
            nonce: String,
        }

        // GET handler for URL verification
        async fn handle_verify(
            State(state): State<WebhookState>,
            Query(params): Query<VerifyParams>,
        ) -> impl IntoResponse {
            let computed = verify_wecom_signature(
                &state.verification_token,
                &params.timestamp,
                &params.nonce,
                &params.echostr,
            );
            if computed != params.msg_signature {
                warn!("WeCom verify: signature mismatch");
                return axum::http::Response::builder()
                    .status(403)
                    .body("signature mismatch".to_string())
                    .unwrap();
            }

            match decrypt_wecom_message(&state.aes_key, &params.echostr) {
                Ok((echostr, _corp_id)) => {
                    info!("WeCom: URL verification succeeded");
                    axum::http::Response::builder()
                        .status(200)
                        .body(echostr)
                        .unwrap()
                }
                Err(e) => {
                    warn!("WeCom verify: decryption failed: {e}");
                    axum::http::Response::builder()
                        .status(400)
                        .body("decryption failed".to_string())
                        .unwrap()
                }
            }
        }

        // POST handler for message callbacks
        async fn handle_callback(
            State(state): State<WebhookState>,
            Query(params): Query<CallbackParams>,
            body: String,
        ) -> impl IntoResponse {
            // Extract Encrypt field from outer XML
            let encrypt_msg = match xml_extract(&body, "Encrypt") {
                Some(e) => e,
                None => {
                    warn!("WeCom callback: no <Encrypt> in body");
                    return axum::http::Response::builder()
                        .status(400)
                        .body("no Encrypt field".to_string())
                        .unwrap();
                }
            };

            // Verify signature
            let computed = verify_wecom_signature(
                &state.verification_token,
                &params.timestamp,
                &params.nonce,
                &encrypt_msg,
            );
            if computed != params.msg_signature {
                warn!("WeCom callback: signature mismatch");
                return axum::http::Response::builder()
                    .status(403)
                    .body("signature mismatch".to_string())
                    .unwrap();
            }

            // Decrypt
            match decrypt_wecom_message(&state.aes_key, &encrypt_msg) {
                Ok((xml_content, ref received_corp_id)) if *received_corp_id == state.corp_id => {
                    let _ = state.inbound_tx.send(xml_content).await;
                    axum::http::Response::builder()
                        .status(200)
                        .body("success".to_string())
                        .unwrap()
                }
                Ok((_, corp_id)) => {
                    warn!(corp_id, "WeCom callback: corp_id mismatch");
                    axum::http::Response::builder()
                        .status(403)
                        .body("corp_id mismatch".to_string())
                        .unwrap()
                }
                Err(e) => {
                    warn!("WeCom callback: decryption failed: {e}");
                    axum::http::Response::builder()
                        .status(400)
                        .body("decryption failed".to_string())
                        .unwrap()
                }
            }
        }

        // Internal channel for passing decrypted XML messages
        let (event_tx, mut event_rx) = mpsc::channel::<String>(100);

        let state = WebhookState {
            verification_token: self.verification_token.clone(),
            aes_key: self.aes_key,
            corp_id: self.corp_id.clone(),
            inbound_tx: event_tx,
        };

        let app = Router::new()
            .route("/wecom/webhook", get(handle_verify).post(handle_callback))
            .with_state(state);

        let addr = format!("0.0.0.0:{}", self.webhook_port);
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .wrap_err_with(|| format!("failed to bind WeCom webhook server to {addr}"))?;
        info!(port = self.webhook_port, "WeCom webhook server listening");

        let shutdown = self.shutdown.clone();

        // Spawn the HTTP server
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

        // Process incoming decrypted XML messages
        while let Some(xml) = event_rx.recv().await {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }

            if let Some(inbound) = self.parse_message(&xml).await {
                info!(
                    sender = %inbound.sender_id,
                    "WeCom: sending to inbound bus"
                );
                if inbound_tx.send(inbound).await.is_err() {
                    error!("WeCom: inbound_tx send failed (receiver dropped)");
                    break;
                }
            }
        }

        Ok(())
    }
}

#[async_trait]
impl Channel for WeComChannel {
    fn name(&self) -> &str {
        "wecom"
    }

    async fn start(&self, inbound_tx: mpsc::Sender<InboundMessage>) -> Result<()> {
        info!("Starting WeCom channel (webhook mode)");
        self.start_webhook(inbound_tx).await?;
        info!("WeCom channel stopped");
        Ok(())
    }

    async fn send(&self, msg: &OutboundMessage) -> Result<()> {
        // Send text content as markdown
        if !msg.content.is_empty() {
            let body = serde_json::json!({
                "msgtype": "markdown",
                "markdown": {
                    "content": msg.content,
                }
            });
            self.send_wecom_message(&msg.chat_id, &body).await?;
        }

        // Send media files
        for path in &msg.media {
            if is_image(path) {
                match self.upload_media(path, "image").await {
                    Ok(media_id) => {
                        let body = serde_json::json!({
                            "msgtype": "image",
                            "image": { "media_id": media_id }
                        });
                        self.send_wecom_message(&msg.chat_id, &body).await?;
                    }
                    Err(e) => warn!("failed to upload WeCom image: {e}"),
                }
            } else {
                match self.upload_media(path, "file").await {
                    Ok(media_id) => {
                        let body = serde_json::json!({
                            "msgtype": "file",
                            "file": { "media_id": media_id }
                        });
                        self.send_wecom_message(&msg.chat_id, &body).await?;
                    }
                    Err(e) => warn!("failed to upload WeCom file: {e}"),
                }
            }
        }

        Ok(())
    }

    fn is_allowed(&self, sender_id: &str) -> bool {
        self.check_allowed(sender_id)
    }

    fn max_message_length(&self) -> usize {
        2048
    }

    async fn stop(&self) -> Result<()> {
        self.shutdown.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wecom_crypto::decode_aes_key;

    fn make_channel(allowed: Vec<&str>) -> WeComChannel {
        let test_key = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG";
        WeComChannel {
            corp_id: "test_corp".into(),
            agent_id: "1000002".into(),
            agent_secret: "test_secret".into(),
            verification_token: "test_token".into(),
            aes_key: decode_aes_key(test_key).unwrap_or([0u8; 32]),
            allowed_senders: allowed.into_iter().map(String::from).collect(),
            shutdown: Arc::new(AtomicBool::new(false)),
            http: Client::new(),
            media_dir: PathBuf::from("/tmp/test-wecom-media"),
            token_cache: Arc::new(tokio::sync::Mutex::new(None)),
            seen_ids: Arc::new(std::sync::Mutex::new(HashSet::new())),
            webhook_port: 9322,
        }
    }

    #[test]
    fn test_dedup() {
        let ch = make_channel(vec![]);
        assert!(!ch.dedup_check("msg1"));
        assert!(ch.dedup_check("msg1")); // duplicate
        assert!(!ch.dedup_check("msg2"));
    }

    #[test]
    fn test_dedup_overflow_clears() {
        let ch = make_channel(vec![]);
        for i in 0..MAX_SEEN_IDS {
            ch.dedup_check(&format!("msg_{i}"));
        }
        assert!(!ch.dedup_check("new_msg"));
        assert!(!ch.dedup_check("msg_0"));
    }

    #[test]
    fn test_is_allowed_empty_list() {
        let ch = make_channel(vec![]);
        assert!(ch.is_allowed("anyone"));
    }

    #[test]
    fn test_is_allowed_matching() {
        let ch = make_channel(vec!["user1", "user2"]);
        assert!(ch.is_allowed("user1"));
        assert!(!ch.is_allowed("user3"));
    }

    #[test]
    fn test_with_webhook_port() {
        let ch = WeComChannel::new(
            "corp",
            "1000002",
            "secret",
            "token",
            "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG",
            vec![],
            Arc::new(AtomicBool::new(false)),
            PathBuf::from("/tmp"),
        )
        .with_webhook_port(8080);
        assert_eq!(ch.webhook_port, 8080);
    }

    #[test]
    fn test_channel_name() {
        let ch = make_channel(vec![]);
        assert_eq!(ch.name(), "wecom");
    }

    #[test]
    fn test_max_message_length() {
        let ch = make_channel(vec![]);
        assert_eq!(ch.max_message_length(), 2048);
    }
}
