//! Gateway message types for channel-based communication.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::SessionKey;

/// Inbound message from a channel to the agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundMessage {
    pub channel: String,
    pub sender_id: String,
    pub chat_id: String,
    pub content: String,
    pub timestamp: DateTime<Utc>,
    #[serde(default)]
    pub media: Vec<String>,
    #[serde(default = "default_metadata")]
    pub metadata: serde_json::Value,
    /// Platform message ID (e.g. Telegram msg.id) for threading replies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
}

/// Outbound message from the agent to a channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboundMessage {
    pub channel: String,
    pub chat_id: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    #[serde(default)]
    pub media: Vec<String>,
    #[serde(default = "default_metadata")]
    pub metadata: serde_json::Value,
}

fn default_metadata() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

impl InboundMessage {
    pub fn session_key(&self) -> SessionKey {
        SessionKey::new(&self.channel, &self.chat_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inbound_session_key() {
        let msg = InboundMessage {
            channel: "telegram".into(),
            sender_id: "user1".into(),
            chat_id: "chat42".into(),
            content: "hello".into(),
            timestamp: Utc::now(),
            media: vec![],
            metadata: serde_json::json!({}),
            message_id: None,
        };
        assert_eq!(msg.session_key(), SessionKey::new("telegram", "chat42"));
    }

    #[test]
    fn test_inbound_serde_roundtrip() {
        let msg = InboundMessage {
            channel: "cli".into(),
            sender_id: "local".into(),
            chat_id: "default".into(),
            content: "test message".into(),
            timestamp: Utc::now(),
            media: vec!["image.png".into()],
            metadata: serde_json::json!({"key": "value"}),
            message_id: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: InboundMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.content, "test message");
        assert_eq!(parsed.media.len(), 1);
    }

    #[test]
    fn test_outbound_serde_roundtrip() {
        let msg = OutboundMessage {
            channel: "cli".into(),
            chat_id: "default".into(),
            content: "response".into(),
            reply_to: Some("msg-1".into()),
            media: vec![],
            metadata: serde_json::json!({}),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: OutboundMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.content, "response");
        assert_eq!(parsed.reply_to, Some("msg-1".into()));
    }

    #[test]
    fn test_inbound_default_metadata() {
        let json = r#"{
            "channel": "cli",
            "sender_id": "local",
            "chat_id": "default",
            "content": "hi",
            "timestamp": "2024-01-01T00:00:00Z"
        }"#;
        let msg: InboundMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.media.len(), 0);
        assert!(msg.metadata.is_object());
    }
}
