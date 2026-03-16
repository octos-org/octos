//! Slack channel using Socket Mode (WebSocket + HTTP).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use chrono::Utc;
use octos_core::{InboundMessage, OutboundMessage};
use eyre::{Result, WrapErr};
use futures::{SinkExt, StreamExt};
use reqwest::Client;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{debug, error, info, warn};

use crate::channel::Channel;

pub struct SlackChannel {
    bot_token: String,
    app_token: String,
    allowed_senders: HashSet<String>,
    shutdown: Arc<AtomicBool>,
    http: Client,
    media_dir: PathBuf,
}

impl SlackChannel {
    pub fn new(
        bot_token: &str,
        app_token: &str,
        allowed_senders: Vec<String>,
        shutdown: Arc<AtomicBool>,
        media_dir: PathBuf,
    ) -> Self {
        Self {
            bot_token: bot_token.to_string(),
            app_token: app_token.to_string(),
            allowed_senders: allowed_senders.into_iter().collect(),
            shutdown,
            http: Client::new(),
            media_dir,
        }
    }

    fn check_allowed(&self, sender_id: &str) -> bool {
        self.allowed_senders.is_empty() || self.allowed_senders.contains(sender_id)
    }

    /// Get WebSocket URL via apps.connections.open.
    async fn get_ws_url(&self) -> Result<String> {
        let resp: serde_json::Value = self
            .http
            .post("https://slack.com/api/apps.connections.open")
            .header("Authorization", format!("Bearer {}", self.app_token))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .send()
            .await
            .wrap_err("failed to call apps.connections.open")?
            .json()
            .await
            .wrap_err("failed to parse apps.connections.open response")?;

        if resp.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            let err = resp
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            eyre::bail!("Slack apps.connections.open failed: {err}");
        }

        resp.get("url")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| eyre::eyre!("no url in apps.connections.open response"))
    }

    /// Resolve the bot's own user ID via auth.test.
    async fn resolve_bot_id(&self) -> Result<String> {
        let resp: serde_json::Value = self
            .http
            .post("https://slack.com/api/auth.test")
            .header("Authorization", format!("Bearer {}", self.bot_token))
            .send()
            .await
            .wrap_err("failed to call auth.test")?
            .json()
            .await?;

        resp.get("user_id")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| eyre::eyre!("no user_id in auth.test response"))
    }

    /// Post a message via chat.postMessage.
    async fn post_message(&self, channel: &str, text: &str, thread_ts: Option<&str>) -> Result<()> {
        let mut body = serde_json::json!({
            "channel": channel,
            "text": text,
        });
        if let Some(ts) = thread_ts {
            body["thread_ts"] = serde_json::Value::String(ts.to_string());
        }

        let resp: serde_json::Value = self
            .http
            .post("https://slack.com/api/chat.postMessage")
            .header("Authorization", format!("Bearer {}", self.bot_token))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .wrap_err("failed to call chat.postMessage")?
            .json()
            .await?;

        if resp.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            let err = resp
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            warn!("chat.postMessage failed: {err}");
        }

        Ok(())
    }
}

#[derive(Deserialize)]
struct SlackEnvelope {
    envelope_id: Option<String>,
    #[serde(default)]
    r#type: String,
    payload: Option<serde_json::Value>,
}

#[async_trait]
impl Channel for SlackChannel {
    fn name(&self) -> &str {
        "slack"
    }

    fn max_message_length(&self) -> usize {
        3900
    }

    async fn start(&self, inbound_tx: mpsc::Sender<InboundMessage>) -> Result<()> {
        info!("Starting Slack channel (Socket Mode)");

        let bot_user_id = match self.resolve_bot_id().await {
            Ok(id) => {
                info!(bot_user_id = %id, "Slack bot connected");
                Some(id)
            }
            Err(e) => {
                warn!("Failed to resolve bot user ID: {e}");
                None
            }
        };

        loop {
            if self.shutdown.load(Ordering::Relaxed) {
                break;
            }

            let ws_url = match self.get_ws_url().await {
                Ok(url) => url,
                Err(e) => {
                    error!("Failed to get Slack WS URL: {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
            };

            let (ws_stream, _) = match connect_async(&ws_url).await {
                Ok(conn) => conn,
                Err(e) => {
                    error!("Failed to connect Slack WebSocket: {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
            };

            info!("Slack WebSocket connected");
            let (mut ws_tx, mut ws_rx) = ws_stream.split();

            while let Some(frame) = ws_rx.next().await {
                if self.shutdown.load(Ordering::Relaxed) {
                    break;
                }

                let data = match frame {
                    Ok(WsMessage::Text(text)) => text,
                    Ok(WsMessage::Close(_)) => {
                        info!("Slack WebSocket closed by server");
                        break;
                    }
                    Ok(_) => continue,
                    Err(e) => {
                        warn!("Slack WebSocket error: {e}");
                        break;
                    }
                };

                let envelope: SlackEnvelope = match serde_json::from_str(&data) {
                    Ok(e) => e,
                    Err(e) => {
                        debug!("Failed to parse Slack envelope: {e}");
                        continue;
                    }
                };

                // Acknowledge the envelope
                if let Some(ref eid) = envelope.envelope_id {
                    let ack = serde_json::json!({ "envelope_id": eid }).to_string();
                    if let Err(e) = ws_tx.send(WsMessage::Text(ack.into())).await {
                        warn!("Failed to ack Slack envelope: {e}");
                        break;
                    }
                }

                if envelope.r#type != "events_api" {
                    continue;
                }

                let Some(payload) = envelope.payload else {
                    continue;
                };
                let Some(event) = payload.get("event") else {
                    continue;
                };

                let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if event_type != "message" && event_type != "app_mention" {
                    continue;
                }

                // Skip bot/system messages (any subtype)
                if event.get("subtype").is_some() {
                    continue;
                }

                let sender_id = event.get("user").and_then(|v| v.as_str()).unwrap_or("");
                let chat_id = event.get("channel").and_then(|v| v.as_str()).unwrap_or("");
                let text = event.get("text").and_then(|v| v.as_str()).unwrap_or("");

                if sender_id.is_empty() || chat_id.is_empty() || text.is_empty() {
                    continue;
                }

                // Skip messages from the bot itself
                if bot_user_id.as_deref() == Some(sender_id) {
                    continue;
                }

                // Deduplicate: skip "message" events that contain a bot mention
                // (Slack sends both message + app_mention for mentions)
                if event_type == "message" {
                    if let Some(ref bid) = bot_user_id {
                        if text.contains(&format!("<@{bid}>")) {
                            continue;
                        }
                    }
                }

                if !self.check_allowed(sender_id) {
                    continue;
                }

                // Strip bot mention from text
                let clean_text = if let Some(ref bid) = bot_user_id {
                    text.replace(&format!("<@{bid}> "), "")
                        .replace(&format!("<@{bid}>"), "")
                        .trim()
                        .to_string()
                } else {
                    text.to_string()
                };

                // Download files if present
                let mut media = Vec::new();
                if let Some(files) = event.get("files").and_then(|v| v.as_array()) {
                    let auth = format!("Bearer {}", self.bot_token);
                    for file in files {
                        let url = file.get("url_private_download").and_then(|v| v.as_str());
                        let name = file.get("name").and_then(|v| v.as_str()).unwrap_or("file");
                        let file_id = file.get("id").and_then(|v| v.as_str()).unwrap_or("unknown");
                        if let Some(url) = url {
                            let ext = std::path::Path::new(name)
                                .extension()
                                .and_then(|e| e.to_str())
                                .map(|e| format!(".{e}"))
                                .unwrap_or_default();
                            let filename = format!("{file_id}{ext}");
                            match crate::media::download_media(
                                &self.http,
                                url,
                                &[("Authorization", auth.as_str())],
                                &self.media_dir,
                                &filename,
                            )
                            .await
                            {
                                Ok(path) => media.push(path.display().to_string()),
                                Err(e) => warn!("failed to download Slack file: {e}"),
                            }
                        }
                    }
                }

                let thread_ts = event
                    .get("thread_ts")
                    .or_else(|| event.get("ts"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                let channel_type = event
                    .get("channel_type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                let inbound = InboundMessage {
                    channel: "slack".into(),
                    sender_id: sender_id.to_string(),
                    chat_id: chat_id.to_string(),
                    content: clean_text,
                    timestamp: Utc::now(),
                    media,
                    metadata: serde_json::json!({
                        "slack": {
                            "thread_ts": thread_ts,
                            "channel_type": channel_type,
                        }
                    }),
                    message_id: None,
                };

                if inbound_tx.send(inbound).await.is_err() {
                    return Ok(());
                }
            }

            if self.shutdown.load(Ordering::Relaxed) {
                break;
            }

            // Reconnect after disconnect
            warn!("Slack WebSocket disconnected, reconnecting in 2s...");
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }

        info!("Slack channel stopped");
        Ok(())
    }

    async fn send(&self, msg: &OutboundMessage) -> Result<()> {
        let slack_meta = msg.metadata.get("slack");
        let thread_ts = slack_meta
            .and_then(|m| m.get("thread_ts"))
            .and_then(|v| v.as_str());
        let channel_type = slack_meta
            .and_then(|m| m.get("channel_type"))
            .and_then(|v| v.as_str());

        // Only thread-reply in channels, not DMs
        let use_thread = thread_ts.is_some() && channel_type != Some("im");

        self.post_message(
            &msg.chat_id,
            &msg.content,
            if use_thread { thread_ts } else { None },
        )
        .await
    }

    fn is_allowed(&self, sender_id: &str) -> bool {
        self.check_allowed(sender_id)
    }

    async fn stop(&self) -> Result<()> {
        self.shutdown.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_channel(allowed: Vec<&str>) -> SlackChannel {
        SlackChannel {
            bot_token: "xoxb-test".into(),
            app_token: "xapp-test".into(),
            allowed_senders: allowed.into_iter().map(String::from).collect(),
            shutdown: Arc::new(AtomicBool::new(false)),
            http: Client::new(),
            media_dir: PathBuf::from("/tmp/test-media"),
        }
    }

    #[test]
    fn test_is_allowed_empty_list() {
        let ch = make_channel(vec![]);
        assert!(ch.is_allowed("anyone"));
    }

    #[test]
    fn test_is_allowed_matching() {
        let ch = make_channel(vec!["U123", "U456"]);
        assert!(ch.is_allowed("U123"));
        assert!(ch.is_allowed("U456"));
        assert!(!ch.is_allowed("U789"));
    }

    #[test]
    fn test_envelope_parsing() {
        let json = r#"{
            "envelope_id": "abc123",
            "type": "events_api",
            "payload": {
                "event": {
                    "type": "message",
                    "user": "U123",
                    "channel": "C456",
                    "text": "hello"
                }
            }
        }"#;
        let env: SlackEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(env.envelope_id.unwrap(), "abc123");
        assert_eq!(env.r#type, "events_api");
        assert!(env.payload.is_some());
    }

    #[test]
    fn test_envelope_missing_optional_fields() {
        let json = r#"{"type": "hello"}"#;
        let env: SlackEnvelope = serde_json::from_str(json).unwrap();
        assert!(env.envelope_id.is_none());
        assert_eq!(env.r#type, "hello");
        assert!(env.payload.is_none());
    }

    #[test]
    fn test_envelope_default_type() {
        let json = r#"{}"#;
        let env: SlackEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(env.r#type, "");
    }

    #[test]
    fn test_channel_name() {
        let ch = make_channel(vec![]);
        assert_eq!(ch.name(), "slack");
    }

    #[test]
    fn test_max_message_length() {
        let ch = make_channel(vec![]);
        assert_eq!(ch.max_message_length(), 3900);
    }
}
