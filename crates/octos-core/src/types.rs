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
/// Longest [`Message::participant`] label kept, in bytes.
pub const MESSAGE_PARTICIPANT_MAX_BYTES: usize = 64;

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
    /// Provenance tag for rows written outside an agent turn
    /// (`session/append_message`, UPCR-2026-042). Turn-written rows leave it
    /// `None`; callers distinguish out-of-band records by `Some(source)`.
    /// Legacy persisted rows omit this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Who spoke in a record written outside an agent turn, when the caller
    /// distinguishes several participants on one side of the conversation
    /// (e.g. two preset tutors answering the same person). One line, at most
    /// [`MESSAGE_PARTICIPANT_MAX_BYTES`]. Memory extraction shows it so that
    /// shared experiences and requests addressed to one participant keep
    /// their scope. Legacy persisted rows omit this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub participant: Option<String>,
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
            source: None,
            participant: None,
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
            source: None,
            participant: None,
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
            source: None,
            participant: None,
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
            source: None,
            participant: None,
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
            source: None,
            participant: None,
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
            source: None,
            participant: None,
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
            source: None,
            participant: None,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionKey(pub String);

/// Synthetic profile ID used for main-profile session isolation.
pub const MAIN_PROFILE_ID: &str = "_main";

impl SessionKey {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

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

    /// Child key for a fork: the parent's profile + channel with a new
    /// chat id (topic dropped — the child is a fresh conversation). A
    /// raw colon-less parent — the SPA's opaque per-tab handles
    /// (`web-123`) — yields a raw child: treating the whole raw id as a
    /// channel would mint `web-123:<id>` keys, which the raw REST
    /// resume/title/delete fallbacks intentionally exclude.
    ///
    /// Built on `split_base_key`, the same profile/channel boundary the
    /// rest of the key parsing uses. A last-segment replacement is NOT
    /// equivalent: chat ids can contain colons (Matrix rooms —
    /// `matrix:!room:localhost`), so only the registry-backed parse
    /// places the boundary correctly for both colon chat ids and
    /// `{profile}:{channel}:{chat}` keys (codex #1613 r2+r3; the
    /// registry gap for feature-gated channels is fixed in
    /// `is_channel_name`). Single source of truth for
    /// `SessionManager::fork` AND the serve-side pre-checks.
    pub fn fork_child(&self, new_chat_id: &str) -> SessionKey {
        if !self.base_key().contains(':') {
            return SessionKey(new_chat_id.to_string());
        }
        let (profile, channel, _chat) = self.split_base_key();
        match profile {
            Some(profile) => SessionKey(format!("{profile}:{channel}:{new_chat_id}")),
            None => SessionKey(format!("{channel}:{new_chat_id}")),
        }
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

/// Whether `value` is a registered session-key channel name (the
/// middle segment of a `{channel}:{chat}` or `{profile}:{channel}:{chat}`
/// key). Exposed so profile-ID validation can REJECT ids that collide
/// with a channel name: such an id makes `profile:channel:chat`
/// indistinguishable from `channel:chat_with_colons`, which
/// `split_base_key` (and thus `profile_id`/`channel`/`fork_child`)
/// would misparse (codex #1613 r4).
pub fn is_reserved_channel_name(value: &str) -> bool {
    is_channel_name(value)
}

fn is_channel_name(value: &str) -> bool {
    matches!(
        value,
        "acp"
            | "api"
            | "cli"
            | "dingtalk"
            | "discord"
            | "email"
            | "feishu"
            | "line"
            | "local"
            | "matrix"
            | "qq-bot"
            | "slack"
            | "system"
            | "telegram"
            | "test"
            | "twilio"
            | "wechat"
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
    fn fork_child_preserves_profile_and_channel() {
        // channel:chat parent
        assert_eq!(
            SessionKey("local:parent".into()).fork_child("kid").0,
            "local:kid"
        );
        // profile:channel:chat parent keeps BOTH prefixes
        assert_eq!(
            SessionKey("tenant:api:parent".into()).fork_child("kid").0,
            "tenant:api:kid"
        );
        // feature-gated channels parse correctly now that the registry
        // includes them (wechat/line were missing pre-#1613)
        assert_eq!(
            SessionKey("tenant:wechat:parent".into())
                .fork_child("kid")
                .0,
            "tenant:wechat:kid"
        );
        // colon-bearing chat ids (Matrix rooms) must be REPLACED whole,
        // not treated as extra prefix segments
        assert_eq!(
            SessionKey("matrix:!room:localhost".into())
                .fork_child("kid")
                .0,
            "matrix:kid"
        );
        // raw SPA handle (no colon) yields a raw child — NOT
        // "web-123:kid", which the raw REST fallbacks exclude
        assert_eq!(
            SessionKey("web-123".into()).fork_child("web-456").0,
            "web-456"
        );
        // topic parents fork off the base conversation, topic dropped
        assert_eq!(
            SessionKey("local:parent#research".into())
                .fork_child("kid")
                .0,
            "local:kid"
        );
        assert_eq!(
            SessionKey("web-123#research".into())
                .fork_child("web-456")
                .0,
            "web-456"
        );
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
    fn message_source_roundtrips_and_legacy_rows_omit_it() {
        // UPCR-2026-042: out-of-band rows carry `source`; turn-written rows
        // and legacy JSONL keep it absent — and absent means ABSENT on
        // serialize, so row bytes for turn-written messages are unchanged.
        let mut msg = Message::system("recorded answer");
        assert!(msg.source.is_none());
        let bare = serde_json::to_string(&msg).unwrap();
        assert!(!bare.contains("source"), "no source key when None: {bare}");

        msg.source = Some("external_record:whiteboard".to_owned());
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""source":"external_record:whiteboard""#));
        let parsed: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.source.as_deref(), Some("external_record:whiteboard"));

        let legacy: Message = serde_json::from_str(
            r#"{"role":"assistant","content":"hi","timestamp":"2026-04-24T00:00:00Z"}"#,
        )
        .unwrap();
        assert!(legacy.source.is_none());
    }

    #[test]
    fn should_round_trip_participant_and_omit_it_when_absent() {
        // UPCR-2026-053: rows without a participant keep their exact bytes.
        let mut msg = Message::assistant("a mountain road");
        let bare = serde_json::to_string(&msg).unwrap();
        assert!(!bare.contains("participant"), "{bare}");
        msg.participant = Some("豆豆老师".to_owned());
        let parsed: Message = serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
        assert_eq!(parsed.participant.as_deref(), Some("豆豆老师"));
        let legacy: Message = serde_json::from_str(
            r#"{"role":"assistant","content":"hi","timestamp":"2026-04-24T00:00:00Z"}"#,
        )
        .unwrap();
        assert!(legacy.participant.is_none());
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
}
