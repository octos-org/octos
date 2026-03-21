//! Core type definitions.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Unique identifier for a task (UUID v7 for temporal ordering).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskId(pub Uuid);

impl TaskId {
    /// Create a new task ID using UUID v7.
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for TaskId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::str::FromStr for TaskId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

/// Unique identifier for an agent.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId(pub String);

impl AgentId {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A message in the conversation history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: MessageRole,
    pub content: String,
    /// Media file paths (images, audio) attached to this message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub media: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Reasoning/thinking content from thinking models (kimi-k2.5, o1, etc.).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    pub timestamp: DateTime<Utc>,
}

impl Message {
    /// Create a user message.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            content: content.into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            timestamp: Utc::now(),
        }
    }

    /// Create an assistant message.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: content.into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            timestamp: Utc::now(),
        }
    }

    /// Create a system message (used for injecting background results, provider switches, etc.).
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::System,
            content: content.into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            timestamp: Utc::now(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

impl MessageRole {
    /// Return the lowercase string representation.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

impl std::fmt::Display for MessageRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    /// Provider-specific metadata (e.g. Gemini thought_signature).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Reference to an episode in episodic memory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpisodeRef {
    pub id: String,
    pub summary: String,
    pub relevance_score: f32,
}

/// Unique key identifying a conversation session (channel:chat_id).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionKey(pub String);

/// Synthetic profile ID used for main-profile session isolation.
pub const MAIN_PROFILE_ID: &str = "_main";

impl SessionKey {
    pub fn new(channel: &str, chat_id: &str) -> Self {
        Self(format!("{channel}:{chat_id}"))
    }

    /// Create a session key with an explicit profile dimension.
    pub fn with_profile(profile_id: &str, channel: &str, chat_id: &str) -> Self {
        Self(format!("{profile_id}:{channel}:{chat_id}"))
    }

    /// Create a session key with a topic suffix (e.g., `telegram:12345#research`).
    /// Empty topic produces the same key as `new()`.
    pub fn with_topic(channel: &str, chat_id: &str, topic: &str) -> Self {
        if topic.is_empty() {
            Self::new(channel, chat_id)
        } else {
            Self(format!("{channel}:{chat_id}#{topic}"))
        }
    }

    /// Create a profiled session key with an optional topic suffix.
    pub fn with_profile_topic(profile_id: &str, channel: &str, chat_id: &str, topic: &str) -> Self {
        if topic.is_empty() {
            Self::with_profile(profile_id, channel, chat_id)
        } else {
            Self(format!("{profile_id}:{channel}:{chat_id}#{topic}"))
        }
    }

    /// Base key without topic: `"telegram:12345#foo"` → `"telegram:12345"`.
    pub fn base_key(&self) -> &str {
        self.0.split('#').next().unwrap_or(&self.0)
    }

    /// Topic suffix if present: `"telegram:12345#foo"` → `Some("foo")`.
    pub fn topic(&self) -> Option<&str> {
        self.0.split_once('#').map(|(_, t)| t)
    }

    fn split_base_key(&self) -> (Option<&str>, &str, &str) {
        let base = self.base_key();
        let mut parts = base.splitn(3, ':');
        let first = parts.next().unwrap_or("");
        let second = parts.next().unwrap_or("");
        let third = parts.next();

        if let Some(rest) = third {
            if !is_channel_name(first) && is_channel_name(second) {
                (Some(first), second, rest)
            } else {
                (None, first, &base[first.len() + 1..])
            }
        } else {
            (None, first, second)
        }
    }

    /// Profile ID when the key is in `{profile}:{channel}:{chat_id}` form.
    pub fn profile_id(&self) -> Option<&str> {
        self.split_base_key().0
    }

    /// Channel name: `"telegram:12345#foo"` → `"telegram"`.
    pub fn channel(&self) -> &str {
        self.split_base_key().1
    }

    /// Chat ID: `"telegram:12345#foo"` → `"12345"`.
    pub fn chat_id(&self) -> &str {
        self.split_base_key().2
    }
}

fn is_channel_name(value: &str) -> bool {
    matches!(
        value,
        "api"
            | "cli"
            | "discord"
            | "email"
            | "feishu"
            | "matrix"
            | "qq-bot"
            | "slack"
            | "system"
            | "telegram"
            | "test"
            | "twilio"
            | "wecom"
            | "wecom-bot"
            | "whatsapp"
    )
}

impl std::fmt::Display for SessionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_id_unique() {
        let id1 = TaskId::new();
        let id2 = TaskId::new();
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_task_id_display() {
        let id = TaskId::new();
        let s = id.to_string();
        assert!(!s.is_empty());
        assert!(s.contains('-')); // UUID format
    }

    #[test]
    fn test_task_id_from_str() {
        let id = TaskId::new();
        let s = id.to_string();
        let parsed: TaskId = s.parse().unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn test_agent_id() {
        let id = AgentId::new("worker-1");
        assert_eq!(id.to_string(), "worker-1");
    }

    #[test]
    fn test_message_serialization() {
        let msg = Message {
            role: MessageRole::User,
            content: "Hello".to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            timestamp: Utc::now(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("Hello"));
        assert!(json.contains("user"));

        // tool_calls should be skipped when None
        assert!(!json.contains("tool_calls"));
        // empty media should be skipped
        assert!(!json.contains("media"));
    }

    #[test]
    fn test_session_key_new() {
        let key = SessionKey::new("telegram", "12345");
        assert_eq!(key.0, "telegram:12345");
    }

    #[test]
    fn test_session_key_display() {
        let key = SessionKey::new("cli", "default");
        assert_eq!(key.to_string(), "cli:default");
    }

    #[test]
    fn test_session_key_equality() {
        let k1 = SessionKey::new("cli", "a");
        let k2 = SessionKey::new("cli", "a");
        let k3 = SessionKey::new("cli", "b");
        assert_eq!(k1, k2);
        assert_ne!(k1, k3);
    }

    #[test]
    fn test_session_key_serde_roundtrip() {
        let key = SessionKey::new("discord", "guild:123");
        let json = serde_json::to_string(&key).unwrap();
        let parsed: SessionKey = serde_json::from_str(&json).unwrap();
        assert_eq!(key, parsed);
    }

    #[test]
    fn test_session_key_with_topic() {
        let key = SessionKey::with_topic("telegram", "12345", "research");
        assert_eq!(key.0, "telegram:12345#research");
        assert_eq!(key.base_key(), "telegram:12345");
        assert_eq!(key.topic(), Some("research"));
        assert_eq!(key.channel(), "telegram");
        assert_eq!(key.chat_id(), "12345");
    }

    #[test]
    fn test_session_key_with_empty_topic() {
        let key = SessionKey::with_topic("telegram", "12345", "");
        assert_eq!(key.0, "telegram:12345");
        assert_eq!(key.topic(), None);
    }

    #[test]
    fn test_session_key_base_key_no_topic() {
        let key = SessionKey::new("whatsapp", "abc");
        assert_eq!(key.base_key(), "whatsapp:abc");
        assert_eq!(key.topic(), None);
        assert_eq!(key.channel(), "whatsapp");
        assert_eq!(key.chat_id(), "abc");
    }

    #[test]
    fn test_session_key_with_profile() {
        let key = SessionKey::with_profile("weather", "matrix", "!room:localhost");
        assert_eq!(key.0, "weather:matrix:!room:localhost");
        assert_eq!(key.profile_id(), Some("weather"));
        assert_eq!(key.base_key(), "weather:matrix:!room:localhost");
        assert_eq!(key.channel(), "matrix");
        assert_eq!(key.chat_id(), "!room:localhost");
    }

    #[test]
    fn test_session_key_with_profile_and_topic() {
        let key = SessionKey::with_profile_topic("weather", "matrix", "!room:localhost", "ops");
        assert_eq!(key.0, "weather:matrix:!room:localhost#ops");
        assert_eq!(key.profile_id(), Some("weather"));
        assert_eq!(key.base_key(), "weather:matrix:!room:localhost");
        assert_eq!(key.topic(), Some("ops"));
        assert_eq!(key.channel(), "matrix");
        assert_eq!(key.chat_id(), "!room:localhost");
    }

    #[test]
    fn test_session_key_with_profile_supports_qq_bot_channel() {
        let key = SessionKey::with_profile("weather", "qq-bot", "group:123");
        assert_eq!(key.profile_id(), Some("weather"));
        assert_eq!(key.channel(), "qq-bot");
        assert_eq!(key.chat_id(), "group:123");
    }

    #[test]
    fn test_session_key_legacy_shape_has_no_profile() {
        let key = SessionKey::new("telegram", "12345");
        assert_eq!(key.profile_id(), None);
        assert_eq!(key.channel(), "telegram");
        assert_eq!(key.chat_id(), "12345");
    }

    #[test]
    fn test_session_key_legacy_chat_id_with_colon_stays_legacy() {
        let key = SessionKey::new("discord", "guild:123");
        assert_eq!(key.profile_id(), None);
        assert_eq!(key.channel(), "discord");
        assert_eq!(key.chat_id(), "guild:123");
    }

    #[test]
    fn test_message_role_as_str_and_display() {
        assert_eq!(MessageRole::System.as_str(), "system");
        assert_eq!(MessageRole::User.as_str(), "user");
        assert_eq!(MessageRole::Assistant.as_str(), "assistant");
        assert_eq!(MessageRole::Tool.as_str(), "tool");
        assert_eq!(MessageRole::User.to_string(), "user");
    }

    #[test]
    fn test_episode_ref() {
        let ep_ref = EpisodeRef {
            id: "ep-123".to_string(),
            summary: "Fixed auth bug".to_string(),
            relevance_score: 0.85,
        };
        let json = serde_json::to_string(&ep_ref).unwrap();
        assert!(json.contains("ep-123"));
        assert!(json.contains("0.85"));
    }
}
