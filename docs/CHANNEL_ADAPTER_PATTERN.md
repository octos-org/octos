# Channel Adapter Pattern

Based on analysis of [openclaw/openclaw](https://github.com/openclaw/openclaw) (Mar 2026).

crew-rs currently uses a single monolithic `Channel` trait (14 methods in `crew-bus/src/channel.rs`). OpenClaw decomposes channel behavior into 14+ fine-grained adapter traits, each handling one concern. This document proposes adopting the pattern for crew-rs.

---

## Problem with the Monolithic Trait

```rust
// Current crew-rs — one trait, all methods
#[async_trait]
pub trait Channel: Send + Sync {
    fn name(&self) -> &str;
    async fn start(&self, tx: mpsc::Sender<InboundMessage>) -> Result<()>;
    async fn send(&self, msg: &OutboundMessage) -> Result<()>;
    fn is_allowed(&self, sender_id: &str) -> bool;
    fn max_message_length(&self) -> usize;
    async fn stop(&self) -> Result<()>;
    async fn send_typing(&self, chat_id: &str) -> Result<()>;
    async fn send_listening(&self, chat_id: &str) -> Result<()>;
    async fn send_with_id(&self, msg: &OutboundMessage) -> Result<Option<String>>;
    async fn edit_message(&self, ...) -> Result<()>;
    async fn delete_message(&self, ...) -> Result<()>;
    async fn edit_message_with_metadata(&self, ...) -> Result<()>;
}
```

**Issues:**
- Every channel must implement every method, even if it doesn't apply (IRC has no reactions, Nostr has no media)
- Adding a new concern (reactions, polls, health checks) requires touching every channel
- No way to query at runtime which capabilities a channel supports
- Methods accumulate over time, trait becomes unwieldy

---

## Proposed Adapter Traits

Decompose into focused traits. A channel implements only the adapters relevant to it.

### Core (required)

```rust
/// Every channel must implement this.
pub trait ChannelCore: Send + Sync {
    fn name(&self) -> &str;
    fn max_message_length(&self) -> usize;
    async fn start(&self, tx: mpsc::Sender<InboundMessage>) -> Result<()>;
    async fn stop(&self) -> Result<()>;
    async fn send(&self, msg: &OutboundMessage) -> Result<()>;
}
```

### Optional Adapters

```rust
/// Health monitoring and connection status.
pub trait StatusAdapter: Send + Sync {
    async fn health_check(&self) -> HealthStatus;
    fn connection_state(&self) -> ConnectionState;
    fn collect_issues(&self) -> Vec<StatusIssue>;
}

/// Access control: who can talk to the bot.
pub trait SecurityAdapter: Send + Sync {
    fn is_allowed(&self, sender_id: &str) -> bool;
    fn dm_policy(&self) -> DmPolicy;
    fn resolve_allow_from(&self) -> Vec<String>;
}

/// Group/guild-specific behavior.
pub trait GroupAdapter: Send + Sync {
    fn requires_mention(&self, group_id: &str) -> bool;
    fn group_tool_policy(&self, group_id: &str) -> Option<ToolPolicy>;
}

/// Thread and topic handling.
pub trait ThreadingAdapter: Send + Sync {
    fn is_thread_reply(&self, msg: &InboundMessage) -> bool;
    fn resolve_thread_id(&self, msg: &InboundMessage) -> Option<String>;
}

/// Emoji reactions.
pub trait ReactionAdapter: Send + Sync {
    async fn add_reaction(&self, chat_id: &str, message_id: &str, emoji: &str) -> Result<()>;
    async fn remove_reaction(&self, chat_id: &str, message_id: &str, emoji: &str) -> Result<()>;
}

/// Native polls.
pub trait PollAdapter: Send + Sync {
    fn max_poll_options(&self) -> usize;
    async fn send_poll(&self, chat_id: &str, question: &str, options: &[String]) -> Result<()>;
}

/// Platform-native commands (slash commands, bot commands).
pub trait CommandAdapter: Send + Sync {
    async fn register_commands(&self) -> Result<()>;
    fn command_prefix(&self) -> &str;  // "/" for Slack/Discord, "/" for Telegram
}

/// Message editing and deletion.
pub trait EditAdapter: Send + Sync {
    async fn edit_message(&self, chat_id: &str, message_id: &str, content: &str) -> Result<()>;
    async fn delete_message(&self, chat_id: &str, message_id: &str) -> Result<()>;
}

/// Real-time streaming (progressive message updates).
pub trait StreamingAdapter: Send + Sync {
    async fn start_stream(&self, chat_id: &str, thread_id: Option<&str>) -> Result<StreamHandle>;
    async fn append_stream(&self, handle: &StreamHandle, text: &str) -> Result<()>;
    async fn stop_stream(&self, handle: StreamHandle) -> Result<()>;
}

/// Typing / presence indicators.
pub trait PresenceAdapter: Send + Sync {
    async fn send_typing(&self, chat_id: &str) -> Result<()>;
    async fn send_listening(&self, chat_id: &str) -> Result<()>;
}

/// Channel-specific system prompt injection.
pub trait AgentPromptAdapter: Send + Sync {
    fn system_prompt_hint(&self) -> Option<String>;
}

/// Mention handling.
pub trait MentionAdapter: Send + Sync {
    fn strip_bot_mention(&self, text: &str) -> String;
    fn format_mention(&self, user_id: &str) -> String;
}

/// Contact/member directory enumeration.
pub trait DirectoryAdapter: Send + Sync {
    async fn list_members(&self, group_id: &str) -> Result<Vec<MemberInfo>>;
    async fn list_groups(&self) -> Result<Vec<GroupInfo>>;
}

/// QR/device linking (WhatsApp, Signal).
pub trait AuthAdapter: Send + Sync {
    async fn start_login(&self) -> Result<LoginChallenge>;
    async fn wait_for_login(&self, challenge: &LoginChallenge) -> Result<()>;
    async fn logout(&self) -> Result<()>;
}

/// Heartbeat delivery (periodic check-ins).
pub trait HeartbeatAdapter: Send + Sync {
    fn is_ready(&self) -> bool;
    fn resolve_recipients(&self) -> Vec<String>;
}
```

---

## Channel Registration

```rust
/// Runtime capability query.
pub struct ChannelCapabilities {
    pub has_status: bool,
    pub has_security: bool,
    pub has_groups: bool,
    pub has_threading: bool,
    pub has_reactions: bool,
    pub has_polls: bool,
    pub has_commands: bool,
    pub has_editing: bool,
    pub has_streaming: bool,
    pub has_presence: bool,
    pub has_directory: bool,
    pub has_auth: bool,
    pub has_heartbeat: bool,
}

/// Each channel declares its capabilities at registration.
pub struct RegisteredChannel {
    pub core: Arc<dyn ChannelCore>,
    pub status: Option<Arc<dyn StatusAdapter>>,
    pub security: Option<Arc<dyn SecurityAdapter>>,
    pub groups: Option<Arc<dyn GroupAdapter>>,
    pub threading: Option<Arc<dyn ThreadingAdapter>>,
    pub reactions: Option<Arc<dyn ReactionAdapter>>,
    pub polls: Option<Arc<dyn PollAdapter>>,
    pub commands: Option<Arc<dyn CommandAdapter>>,
    pub editing: Option<Arc<dyn EditAdapter>>,
    pub streaming: Option<Arc<dyn StreamingAdapter>>,
    pub presence: Option<Arc<dyn PresenceAdapter>>,
    pub directory: Option<Arc<dyn DirectoryAdapter>>,
    pub auth: Option<Arc<dyn AuthAdapter>>,
    pub heartbeat: Option<Arc<dyn HeartbeatAdapter>>,
}
```

---

## Per-Channel Coverage

What each channel would implement under this pattern:

| Adapter | Telegram | Discord | Slack | WhatsApp | Signal | Email | CLI |
|---------|----------|---------|-------|----------|--------|-------|-----|
| **Core** | Yes | Yes | Yes | Yes | Yes | Yes | Yes |
| Status | Yes | Yes | Yes | Yes | Yes | Yes | -- |
| Security | Yes | Yes | Yes | Yes | Yes | -- | -- |
| Groups | Yes | Yes | Yes | Yes | -- | -- | -- |
| Threading | Yes | Yes | Yes | -- | -- | Yes | -- |
| Reactions | Yes | Yes | Yes | Yes | Yes | -- | -- |
| Polls | Yes | Yes | -- | Yes | -- | -- | -- |
| Commands | Yes | Yes | Yes | -- | -- | -- | -- |
| Editing | -- | -- | Yes | -- | -- | -- | -- |
| Streaming | Yes | Yes | Yes | -- | Yes | -- | Yes |
| Presence | Yes | Yes | Yes | Yes | -- | -- | -- |
| Directory | -- | Yes | Yes | -- | -- | -- | -- |
| Auth | -- | -- | -- | Yes | Yes | -- | -- |
| Heartbeat | -- | -- | -- | Yes | -- | -- | -- |

---

## Migration Path

1. **Phase 1**: Define the adapter traits alongside the existing `Channel` trait
2. **Phase 2**: Implement adapters for one channel (start with Telegram — most mature)
3. **Phase 3**: Add `RegisteredChannel` to `ChannelManager`, keep backward compat
4. **Phase 4**: Migrate remaining channels one at a time
5. **Phase 5**: Remove the monolithic `Channel` trait

Each phase is independently shippable. No big-bang rewrite.

---

## Benefits

- **Incremental channel development**: New channels start with just `ChannelCore`, add adapters as needed
- **Independent evolution**: Adding reactions doesn't touch Email; adding polls doesn't touch IRC
- **Testability**: Each adapter is independently testable without mocking the full channel
- **Compile-time safety**: Rust's trait system ensures adapters are correctly implemented
- **Runtime capability query**: `ChannelCapabilities` lets the gateway/dashboard know what each channel supports
- **Extension-friendly**: Future plugin channels implement only the adapters they need

---

## Reference

- OpenClaw adapter types: `src/channels/plugins/types.adapters.ts`
- OpenClaw channel registry: `src/channels/registry.ts`
- OpenClaw channel dock: `src/channels/dock.ts`
