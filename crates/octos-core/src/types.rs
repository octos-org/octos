//! Core type definitions.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// Re-export `TurnId` (the protocol identity) so consumers of the typed
// `Message` constructors can pull all three identity newtypes from one place.
pub use crate::ui_protocol::TurnId;

/// Client-supplied message correlation token.
///
/// Carries the optimistic-UI / idempotency identity assigned by a client when
/// it submits a user message. Distinct from [`ThreadId`] (render grouping) and
/// [`TurnId`] (server protocol identity) — see the M8.10 thread-binding bug
/// chain (#649 → #664 → #673 → #680 → #738 → #740) for why these MUST NOT be
/// interchangeable.
///
/// Wraps `String` rather than `Uuid` because clients sometimes mint these as
/// stringified UUIDs, sometimes as opaque tokens. The newtype prevents
/// accidental swaps with [`ThreadId`] at compile time.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientMessageId(pub String);

impl ClientMessageId {
    /// Wrap an existing client-message-id string.
    ///
    /// Accepts any non-empty string. Empty strings are rejected at the
    /// `try_new` boundary; this infallible variant is for paths where the
    /// caller has already validated the input or where the empty case is
    /// statically impossible (e.g. `Uuid::to_string()`).
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Fallible constructor that rejects the empty string.
    ///
    /// Routing code in `session_actor` and `add_message_with_seq` already
    /// treats the empty string as "absent" — guarding here means an upstream
    /// layer that drops a cmid down to `Some("")` cannot accidentally claim
    /// to have a cmid through the type system.
    pub fn try_new(id: impl Into<String>) -> Result<Self, IdentityError> {
        let s = id.into();
        if s.is_empty() {
            Err(IdentityError::Empty {
                kind: IdentityKind::ClientMessageId,
            })
        } else {
            Ok(Self(s))
        }
    }

    /// Mint a fresh ID (used by tests and synthetic call paths).
    pub fn generate() -> Self {
        Self(Uuid::now_v7().to_string())
    }

    /// Borrow the inner string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ClientMessageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for ClientMessageId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// Render-grouping key that pins assistant + tool replies to the originating
/// user bubble.
///
/// Roots a thread on the user message (`thread_id == client_message_id` for
/// the rooting user) and is inherited by assistant/tool replies so the web
/// client can render a chat as `Vec<Thread>` rather than a flat message list.
///
/// Distinct from [`ClientMessageId`] (per-message correlation) and [`TurnId`]
/// (server protocol identity) so the type system rejects swapping them. See
/// PR #742 (`fix(session_actor): pre-stamp thread_id`) for the bug class this
/// newtype structurally prevents.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ThreadId(pub String);

impl ThreadId {
    /// Wrap an existing thread-id string.
    ///
    /// Accepts any non-empty string; see `try_new` for fallible construction
    /// that rejects the empty string.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Fallible constructor that rejects the empty string.
    pub fn try_new(id: impl Into<String>) -> Result<Self, IdentityError> {
        let s = id.into();
        if s.is_empty() {
            Err(IdentityError::Empty {
                kind: IdentityKind::ThreadId,
            })
        } else {
            Ok(Self(s))
        }
    }

    /// Mint a [`ThreadId`] from the [`ClientMessageId`] of the rooting user
    /// message. This is the canonical "thread inherits from the rooting user
    /// message" rule expressed as a named conversion (clearer than the
    /// blanket `From<&ClientMessageId>` impl, which is retained for back-compat).
    pub fn rooted_at(cmid: &ClientMessageId) -> Self {
        Self(cmid.0.clone())
    }

    /// Borrow the inner string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Distinguishes the three identity kinds in [`IdentityError`] messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityKind {
    ClientMessageId,
    ThreadId,
}

impl std::fmt::Display for IdentityKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ClientMessageId => "ClientMessageId",
            Self::ThreadId => "ThreadId",
        })
    }
}

/// Error returned by the fallible `try_new` constructors on the identity
/// newtypes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityError {
    Empty { kind: IdentityKind },
}

impl std::fmt::Display for IdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty { kind } => write!(f, "{kind} cannot be empty"),
        }
    }
}

impl std::error::Error for IdentityError {}

impl std::fmt::Display for ThreadId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for ThreadId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&ClientMessageId> for ThreadId {
    /// A user message roots a thread under its own [`ClientMessageId`]. This
    /// conversion is the canonical "thread inherits from the rooting user
    /// message" rule and is the ONLY infallible bridge between the two
    /// identity types.
    fn from(cmid: &ClientMessageId) -> Self {
        Self(cmid.0.clone())
    }
}

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
    /// Optional client-supplied UUID used to correlate optimistic UI bubbles
    /// to the server-assigned sequence (`historySeq`). Plumbed end-to-end
    /// through `add_message_with_seq` and the `session_result` event so the
    /// web client can stamp the authoritative seq onto the right bubble
    /// without a backfill round-trip. Legacy persisted rows omit this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_message_id: Option<String>,
    /// M8.10 thread grouping key (PR #1). Roots a thread on the user message
    /// (`thread_id == client_message_id`); assistant/tool replies inherit the
    /// same id so the web client can render a chat as `Vec<Thread>` rather
    /// than a flat message list. `None` for system messages and for legacy
    /// rows that pre-date this field — the load path synthesizes a value
    /// in-memory so `Session::threads()` produces sensible groupings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    pub timestamp: DateTime<Utc>,
}

impl Message {
    /// Create a user message with a typed [`ClientMessageId`] attached.
    ///
    /// Preferred constructor for production user-message construction —
    /// requiring the `ClientMessageId` argument means the type system rejects
    /// any code path that forgets to set the correlation token. `thread_id`
    /// stays `None`; the server stamps it (typically equal to the
    /// `ClientMessageId`) when the message is processed inbound — see the
    /// PR-A architecture plan and PR #742.
    ///
    /// If you already know the user message roots its own thread (the common
    /// case), prefer [`Message::user_rooting_thread`] which stamps both
    /// identity tokens in one call.
    pub fn user_with_cmid(content: impl Into<String>, cmid: ClientMessageId) -> Self {
        Self {
            role: MessageRole::User,
            content: content.into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: Some(cmid.0),
            thread_id: None,
            timestamp: Utc::now(),
        }
    }

    /// Create a user message that roots its own thread.
    ///
    /// Convenience constructor for the canonical rule "user message's thread
    /// equals its `client_message_id`" — saves the caller from re-stating the
    /// invariant at every site. This is the recommended entry point for
    /// inbound persistence paths that have a typed [`ClientMessageId`] in
    /// hand. See Codex's PR-A review (`/tmp/codex-pra-review.log`) for the
    /// rationale.
    pub fn user_rooting_thread(content: impl Into<String>, cmid: ClientMessageId) -> Self {
        let thread_id = ThreadId::rooted_at(&cmid);
        Self {
            role: MessageRole::User,
            content: content.into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: Some(cmid.0),
            thread_id: Some(thread_id.0),
            timestamp: Utc::now(),
        }
    }

    /// Create an assistant message bound to an explicit [`ThreadId`].
    ///
    /// Preferred constructor for production assistant-message construction
    /// (e.g. `persist_assistant_message`, late-arriving background results).
    /// Forcing the `ThreadId` argument means callers can no longer silently
    /// fall through to `derive_thread_id_for_new_message`'s "most recent
    /// user" heuristic — the structural fix that closes the #649 → #740 bug
    /// chain.
    pub fn assistant_with_thread(content: impl Into<String>, thread_id: ThreadId) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: content.into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: Some(thread_id.0),
            timestamp: Utc::now(),
        }
    }

    /// Create a tool-result message bound to an explicit [`ThreadId`].
    ///
    /// Tool replies inherit the originating user turn's thread so a
    /// late-arriving tool result lands under the right user bubble even when
    /// later turns have rotated the per-chat sticky `thread_id`.
    pub fn tool_with_thread(
        content: impl Into<String>,
        tool_call_id: impl Into<String>,
        thread_id: ThreadId,
    ) -> Self {
        Self {
            role: MessageRole::Tool,
            content: content.into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
            reasoning_content: None,
            client_message_id: None,
            thread_id: Some(thread_id.0),
            timestamp: Utc::now(),
        }
    }

    /// Create a user message.
    ///
    /// Legacy constructor retained for backwards compatibility with tests,
    /// JSONL deserialization round-trips, and the (~50) call sites that
    /// pre-date PR A's typed constructors. New code on the inbound /
    /// persistence path SHOULD use [`Message::user_with_cmid`] so the
    /// `ClientMessageId` is supplied at the type level — see the PR-A
    /// architecture plan in `/tmp/octos-architecture-FINAL.md` §A.1.
    #[doc(hidden)]
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            content: content.into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: Utc::now(),
        }
    }

    /// Create an assistant message.
    ///
    /// Legacy constructor retained for backwards compatibility. New code
    /// SHOULD use [`Message::assistant_with_thread`] when the originating
    /// thread is known at construction time, so the type system rejects the
    /// "fall through to most-recent-user" misroute path.
    #[doc(hidden)]
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: content.into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: Utc::now(),
        }
    }

    /// Create a system message (used for injecting background results, provider switches, etc.).
    ///
    /// System messages aren't turn-scoped (no thread, no client correlation),
    /// so this constructor stays as the canonical entry point — but is kept
    /// `#[doc(hidden)]` for consistency with the legacy user/assistant pair.
    #[doc(hidden)]
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::System,
            content: content.into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: Utc::now(),
        }
    }

    /// Attach a client-supplied UUID used by the web/runtime client to
    /// correlate optimistic message bubbles back to the persisted seq.
    #[must_use]
    pub fn with_client_message_id(mut self, client_message_id: impl Into<String>) -> Self {
        self.client_message_id = Some(client_message_id.into());
        self
    }

    /// Attach a typed [`ClientMessageId`]. Convenience for paths that already
    /// hold the typed identity (e.g. PR A migrations) so they don't need to
    /// stringify before calling [`Message::with_client_message_id`].
    #[must_use]
    pub fn with_typed_client_message_id(mut self, cmid: ClientMessageId) -> Self {
        self.client_message_id = Some(cmid.0);
        self
    }

    /// Attach a typed [`ThreadId`].
    #[must_use]
    pub fn with_thread_id(mut self, thread_id: ThreadId) -> Self {
        self.thread_id = Some(thread_id.0);
        self
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
            client_message_id: None,
            thread_id: None,
            timestamp: Utc::now(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("Hello"));
        assert!(json.contains("user"));

        // tool_calls should be skipped when None
        assert!(!json.contains("tool_calls"));
        // empty media should be skipped
        assert!(!json.contains("media"));
        // client_message_id should be skipped when None for forward compat
        assert!(!json.contains("client_message_id"));
        // thread_id should be skipped when None for forward compat
        assert!(!json.contains("thread_id"));
    }

    #[test]
    fn message_round_trips_thread_id_when_present() {
        let mut msg = Message::user("hi");
        msg.thread_id = Some("thread-cmid-xyz".to_string());
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("thread_id"));
        assert!(json.contains("thread-cmid-xyz"));

        let parsed: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.thread_id.as_deref(), Some("thread-cmid-xyz"));
    }

    #[test]
    fn message_deserializes_legacy_jsonl_without_thread_id() {
        // Legacy persisted rows pre-dating this field MUST still parse so
        // existing JSONL files don't break the runtime on reload (M8.10 PR #1).
        let legacy = r#"{
            "role": "assistant",
            "content": "ok",
            "timestamp": "2026-04-26T00:00:00Z"
        }"#;
        let msg: Message = serde_json::from_str(legacy).unwrap();
        assert!(msg.thread_id.is_none());
        assert_eq!(msg.content, "ok");
    }

    #[test]
    fn message_round_trips_client_message_id_when_present() {
        let msg = Message::user("hi").with_client_message_id("cmid-abc");
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("client_message_id"));
        assert!(json.contains("cmid-abc"));

        let parsed: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.client_message_id.as_deref(), Some("cmid-abc"));
    }

    #[test]
    fn message_deserializes_legacy_jsonl_without_client_message_id() {
        // Legacy persisted rows pre-dating this field MUST still parse so
        // existing JSONL files don't break the runtime on reload.
        let legacy = r#"{
            "role": "user",
            "content": "hi",
            "timestamp": "2026-04-24T00:00:00Z"
        }"#;
        let msg: Message = serde_json::from_str(legacy).unwrap();
        assert!(msg.client_message_id.is_none());
        assert_eq!(msg.content, "hi");
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

    // PR A: typed identity newtypes — ClientMessageId / ThreadId / TurnId.
    //
    // These tests prove the structural fix that closes the M8.10
    // thread-binding bug class (#649 → #664 → #673 → #680 → #738 → #740): the
    // typed constructors require the identity tokens at the type level, so
    // every code path that constructs a turn-scoped Message MUST supply them
    // (or explicitly opt into the legacy `#[doc(hidden)]` constructors).

    #[test]
    fn client_message_id_round_trips_through_serde() {
        let id = ClientMessageId::new("cmid-abc-123");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"cmid-abc-123\"");
        let parsed: ClientMessageId = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, id);
    }

    #[test]
    fn client_message_id_display_and_as_str_match_inner() {
        let id = ClientMessageId::new("cmid-xyz");
        assert_eq!(id.as_str(), "cmid-xyz");
        assert_eq!(id.to_string(), "cmid-xyz");
    }

    #[test]
    fn client_message_id_generate_yields_unique_values() {
        let a = ClientMessageId::generate();
        let b = ClientMessageId::generate();
        assert_ne!(a, b);
        // UUIDs are 36 chars; we don't pin the exact format, just non-empty.
        assert!(!a.0.is_empty());
    }

    #[test]
    fn thread_id_round_trips_through_serde() {
        let tid = ThreadId::new("thread-cmid-xyz");
        let json = serde_json::to_string(&tid).unwrap();
        assert_eq!(json, "\"thread-cmid-xyz\"");
        let parsed: ThreadId = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, tid);
    }

    #[test]
    fn thread_id_can_be_minted_from_client_message_id() {
        // The canonical "thread inherits from rooting user message" rule.
        let cmid = ClientMessageId::new("cmid-root");
        let tid: ThreadId = (&cmid).into();
        assert_eq!(tid.as_str(), "cmid-root");
        assert_eq!(tid.to_string(), "cmid-root");
    }

    #[test]
    fn message_user_with_cmid_stamps_client_message_id() {
        let cmid = ClientMessageId::new("cmid-001");
        let msg = Message::user_with_cmid("hello", cmid.clone());
        assert_eq!(msg.role, MessageRole::User);
        assert_eq!(msg.content, "hello");
        assert_eq!(msg.client_message_id.as_deref(), Some("cmid-001"));
        // user_with_cmid leaves thread_id None — the server stamps it during
        // process_inbound (typically equal to the cmid). PR-F will tighten
        // this further; PR A only adds the typed entry point.
        assert!(msg.thread_id.is_none());
    }

    #[test]
    fn message_assistant_with_thread_stamps_thread_id() {
        let tid = ThreadId::new("thread-root-1");
        let msg = Message::assistant_with_thread("ack", tid.clone());
        assert_eq!(msg.role, MessageRole::Assistant);
        assert_eq!(msg.content, "ack");
        assert_eq!(msg.thread_id.as_deref(), Some("thread-root-1"));
        // The assistant constructor MUST NOT inherit a client_message_id —
        // that's the user's identity, not the assistant's.
        assert!(msg.client_message_id.is_none());
    }

    #[test]
    fn message_tool_with_thread_carries_call_id_and_thread() {
        let tid = ThreadId::new("thread-root-1");
        let msg = Message::tool_with_thread("ok", "call_42", tid);
        assert_eq!(msg.role, MessageRole::Tool);
        assert_eq!(msg.tool_call_id.as_deref(), Some("call_42"));
        assert_eq!(msg.thread_id.as_deref(), Some("thread-root-1"));
        assert!(msg.client_message_id.is_none());
    }

    #[test]
    fn message_with_typed_client_message_id_attaches() {
        let cmid = ClientMessageId::new("cmid-typed");
        let msg = Message::user("hi").with_typed_client_message_id(cmid);
        assert_eq!(msg.client_message_id.as_deref(), Some("cmid-typed"));
    }

    #[test]
    fn message_with_thread_id_attaches() {
        let tid = ThreadId::new("thread-typed");
        let msg = Message::assistant("hi").with_thread_id(tid);
        assert_eq!(msg.thread_id.as_deref(), Some("thread-typed"));
    }

    #[test]
    fn legacy_message_constructors_still_produce_unstamped_messages() {
        // Back-compat sentinel: PR A keeps the legacy constructors working so
        // tests, JSONL deserialization, and the ~50 not-yet-migrated call
        // sites continue to compile. They simply don't carry the new typed
        // identity guarantees.
        let user = Message::user("plain user");
        assert_eq!(user.role, MessageRole::User);
        assert!(user.client_message_id.is_none());
        assert!(user.thread_id.is_none());

        let assistant = Message::assistant("plain assistant");
        assert_eq!(assistant.role, MessageRole::Assistant);
        assert!(assistant.client_message_id.is_none());
        assert!(assistant.thread_id.is_none());

        let system = Message::system("plain system");
        assert_eq!(system.role, MessageRole::System);
        assert!(system.client_message_id.is_none());
        assert!(system.thread_id.is_none());
    }

    #[test]
    fn typed_message_round_trips_through_jsonl() {
        // Going through serde with a stamped message preserves both identity
        // tokens — proves PR A's typed constructors interoperate with the
        // existing JSONL persistence path without schema changes.
        let cmid = ClientMessageId::new("cmid-jsonl");
        let user = Message::user_with_cmid("q", cmid).with_thread_id(ThreadId::new("thread-jsonl"));
        let json = serde_json::to_string(&user).unwrap();
        let parsed: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.client_message_id.as_deref(), Some("cmid-jsonl"));
        assert_eq!(parsed.thread_id.as_deref(), Some("thread-jsonl"));

        let assistant = Message::assistant_with_thread("a", ThreadId::new("thread-jsonl"));
        let json = serde_json::to_string(&assistant).unwrap();
        let parsed: Message = serde_json::from_str(&json).unwrap();
        assert!(parsed.client_message_id.is_none());
        assert_eq!(parsed.thread_id.as_deref(), Some("thread-jsonl"));
    }

    #[test]
    fn client_message_id_try_new_rejects_empty_string() {
        let err = ClientMessageId::try_new("").unwrap_err();
        assert_eq!(
            err,
            IdentityError::Empty {
                kind: IdentityKind::ClientMessageId
            }
        );
        assert_eq!(err.to_string(), "ClientMessageId cannot be empty");
    }

    #[test]
    fn thread_id_try_new_rejects_empty_string() {
        let err = ThreadId::try_new("").unwrap_err();
        assert_eq!(
            err,
            IdentityError::Empty {
                kind: IdentityKind::ThreadId
            }
        );
        assert_eq!(err.to_string(), "ThreadId cannot be empty");
    }

    #[test]
    fn try_new_accepts_non_empty_strings() {
        let cmid = ClientMessageId::try_new("ok").unwrap();
        assert_eq!(cmid.as_str(), "ok");
        let tid = ThreadId::try_new("ok").unwrap();
        assert_eq!(tid.as_str(), "ok");
    }

    #[test]
    fn thread_id_rooted_at_named_conversion_matches_from_impl() {
        // Both spellings of "thread inherits from rooting user message" must
        // produce identical results — the named conversion is for callers
        // that prefer self-documenting code; the `From` impl is for generic
        // contexts.
        let cmid = ClientMessageId::new("cmid-root");
        let tid_named = ThreadId::rooted_at(&cmid);
        let tid_from: ThreadId = (&cmid).into();
        assert_eq!(tid_named, tid_from);
        assert_eq!(tid_named.as_str(), "cmid-root");
    }

    #[test]
    fn message_user_rooting_thread_stamps_both_identity_tokens() {
        // Codex's PR-A review (/tmp/codex-pra-review.log finding High-2):
        // user-message constructors should be able to root their own thread
        // so the server doesn't have to fall back to the derivation path.
        let cmid = ClientMessageId::new("cmid-root-2");
        let msg = Message::user_rooting_thread("hi", cmid.clone());
        assert_eq!(msg.role, MessageRole::User);
        assert_eq!(msg.client_message_id.as_deref(), Some("cmid-root-2"));
        assert_eq!(msg.thread_id.as_deref(), Some("cmid-root-2"));
        // Sanity: the thread always equals the cmid for a rooted user.
        assert_eq!(msg.thread_id.as_deref(), msg.client_message_id.as_deref());
    }

    #[test]
    fn turn_id_is_distinct_type_from_client_message_id_and_thread_id() {
        // Compile-time proof that the three identity types are NOT
        // interchangeable. If you try to pass one where another is expected
        // the build fails — exactly the structural guarantee PR A is
        // delivering. We also prove it at runtime: serialized representations
        // are different (TurnId is a UUID, the others are opaque strings).
        let turn = TurnId::new();
        let cmid = ClientMessageId::new("cmid-A");
        let tid = ThreadId::new("thread-A");

        // Sanity: each round-trips through its own serde shape.
        let _: TurnId = serde_json::from_str(&serde_json::to_string(&turn).unwrap()).unwrap();
        let _: ClientMessageId =
            serde_json::from_str(&serde_json::to_string(&cmid).unwrap()).unwrap();
        let _: ThreadId = serde_json::from_str(&serde_json::to_string(&tid).unwrap()).unwrap();

        // The TurnId wraps a Uuid (36 chars with dashes); cmid and tid wrap
        // arbitrary strings. They live in different slots of the protocol.
        assert_eq!(turn.0.to_string().len(), 36);
        assert_eq!(cmid.as_str(), "cmid-A");
        assert_eq!(tid.as_str(), "thread-A");
    }
}
