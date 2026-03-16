# Channel Integration Comparison: octos vs OpenClaw

A side-by-side comparison of how octos and OpenClaw integrate with messaging platforms.

## 1. Core Abstraction

### octos: Flat Trait (12 methods)

```rust
pub trait Channel: Send + Sync {
    fn name(&self) -> &str;
    async fn start(&self, inbound_tx: Sender<InboundMessage>) -> Result<()>;
    async fn send(&self, msg: &OutboundMessage) -> Result<()>;
    fn is_allowed(&self, sender_id: &str) -> bool;               // default: true
    fn max_message_length(&self) -> usize;                        // default: 4000
    async fn stop(&self) -> Result<()>;                           // default: no-op
    async fn send_typing(&self, chat_id: &str) -> Result<()>;    // default: no-op
    async fn send_listening(&self, chat_id: &str) -> Result<()>; // default: typing
    async fn send_with_id(&self, msg: &OutboundMessage) -> Result<Option<String>>;
    async fn edit_message(&self, chat_id: &str, msg_id: &str, content: &str) -> Result<()>;
    async fn delete_message(&self, chat_id: &str, msg_id: &str) -> Result<()>;
    async fn edit_message_with_metadata(&self, chat_id: &str, msg_id: &str,
        content: &str, metadata: &Value) -> Result<()>;
}
```

**Strengths:**
- Simple, easy to implement a new channel (~200-500 lines)
- Single binary — no runtime deps
- Type-safe, compile-time checked

**Weaknesses:**
- No multi-account per channel type
- No rich payload abstraction (inline keyboards via metadata hack)
- No health check / probe API
- No group-specific policies

### OpenClaw: Plugin Adapter Pattern (~20 optional slots)

```typescript
ChannelPlugin = {
    config:    ChannelConfigAdapter     // Multi-account (required)
    outbound:  ChannelOutboundAdapter   // Send text/media/polls
    security:  ChannelSecurityAdapter   // 3-level access control
    gateway:   ChannelGatewayAdapter    // Lifecycle per account
    streaming: ChannelStreamingAdapter  // Progressive delivery
    threading: ChannelThreadingAdapter  // Reply/thread context
    groups:    ChannelGroupAdapter      // Group policies
    actions:   ChannelMessageActionAdapter  // Agent-discoverable actions
    status:    ChannelStatusAdapter     // Health probing
    // ... 10+ more optional adapters
}
```

**Strengths:**
- Rich capability declaration
- Multi-account native
- Granular access control (DM + group + action level)
- Health monitoring built-in

**Weaknesses:**
- Complex (~50 files, ~15K lines for channel system alone)
- Node.js runtime required
- Runtime type checking (TypeScript interfaces, not enforced)

## 2. Connection Methods

| Platform | octos | OpenClaw |
|----------|---------|----------|
| **Telegram** | Long polling (teloxide) | Long polling (grammY) |
| **WhatsApp** | WebSocket to Node.js bridge (Baileys sidecar) | Direct Baileys integration (same process) |
| **Discord** | Serenity gateway (WebSocket) | discord.js gateway (WebSocket) |
| **Slack** | Socket Mode (WebSocket) | Socket Mode (WebSocket) |
| **Feishu** | WebSocket (default) OR webhook (port 9321) | N/A (not supported) |
| **WeCom** | Webhook (port 9322) + REST API | N/A (not supported) |
| **Twilio** | Webhook (port 8090) + REST API | N/A (not supported) |
| **Signal** | N/A | signal-cli linked device |
| **Matrix** | N/A | matrix-bot-sdk (extension) |
| **iMessage** | N/A | Apple Messages (WIP) |

### octos unique: Pure Rust Cryptography

octos implements cryptographic primitives (SHA-1, SHA-256, AES-128-CBC, AES-256-CBC, HMAC, Base64) in pure Rust for Feishu, WeCom, and Twilio webhook signature verification. No OpenSSL or external crypto dependency. This is a deliberate design choice for the single-binary deployment model.

### octos unique: WhatsApp via Bridge Architecture

```
WhatsApp ←→ Node.js Bridge (Baileys) ←→ WebSocket ←→ octos
                                              ↓
                               Port 3001 (WS) + Port 3002 (media HTTP)
```

The bridge runs as a separate Node.js process. octos communicates via WebSocket JSON messages (`{"type":"send","to":"...","text":"..."}`). Media files are served via HTTP on port 3002.

OpenClaw embeds Baileys directly in the same Node.js process — simpler deployment but couples to Node.js.

## 3. Inbound Message Pipeline

### octos

```
Platform Event
  → Channel.start() listener (long-running tokio task)
  → Build InboundMessage { channel, sender_id, chat_id, content, media, metadata }
  → inbound_tx.send() → MPSC channel
  → Gateway dispatcher reads from bus
  → Resolve SessionKey (channel:chat_id#topic)
  → ActorRegistry.dispatch() → SessionActor inbox (bounded, 32 messages)
  → Agent.process_message()
```

**Platform-specific handling in channel code:**
- Telegram: Downloads photos/voice/audio/docs, handles callback queries
- WhatsApp: Parses bridge JSON, downloads media from HTTP sidecar
- Feishu: Verifies signature, decrypts AES-CBC, deduplicates by message_id
- Twilio: Verifies HMAC-SHA1 signature, deduplicates by MessageSid
- WeCom: Verifies signature, decrypts AES-128-CBC, extracts from XML
- Slack: Acknowledges envelope, strips bot mentions, downloads private files
- Discord: Filters bot messages, downloads attachments

### OpenClaw

```
Platform Event
  → Channel gateway listener
  → Normalization (platform → standard format)
  → Access Control (DM policy check)
  → Deduplication (debounce window)
  → Message Coalescing (batch rapid-fire messages)
  → Context Enrichment (sender identity, thread context, mention parsing)
  → Queue Policy (run-now / enqueue-followup / drop)
  → Agent Processing
```

### Differences

| Step | octos | OpenClaw |
|------|---------|----------|
| **Normalization** | In-channel (each channel handles its own format) | Centralized normalize layer |
| **Access control** | `is_allowed()` single check | 3-level: DM policy + group policy + action gates |
| **Deduplication** | Feishu/Twilio only (message ID cache, 1000 max) | All channels (configurable debounce window) |
| **Coalescing** | None — each message processed individually | Batch rapid-fire messages into one |
| **Mention stripping** | Slack only (strips bot @mention) | All channels (configurable strip patterns) |
| **Queue policy** | Bounded inbox (32), backpressure notification | Explicit run/enqueue/drop decisions |
| **Thread context** | Slack thread_ts in metadata | All channels with thread/reply awareness |

## 4. Outbound Message Pipeline

### octos

```
Agent Response (ConversationResponse.content)
  → session_actor sends OutboundMessage via proxy_tx
  → Outbound forwarder (active session check, pending buffer)
  → ChannelManager dispatcher
  → split_message() if text exceeds max_message_length
  → Channel.send() (platform-specific)
```

**Message splitting** (`coalesce.rs`):
- Break priority: paragraph > newline > sentence > space > hard char
- Max 50 chunks (truncate with marker if exceeded)
- UTF-8 safe boundary detection

**Platform formatting:**
- Telegram: `markdown_to_telegram_html()` → HTML parse mode
- All others: raw text (no conversion)

### OpenClaw

```
Agent Response
  → Payload Normalization (strip HTML for plain-text channels)
  → Write-Ahead Queue (crash recovery)
  → message_sending hook (can modify/cancel)
  → Text Chunking (per-channel, paragraph-aware)
  → Platform-specific Markdown conversion
  → Channel send (text / media / rich payload)
  → message_sent hook (transcript recording)
  → Queue cleanup (ack on success)
```

**Platform formatting:**
- Telegram: Markdown → HTML (`<b>`, `<i>`, `<code>`)
- WhatsApp: Markdown → WA format (`**bold**` → `*bold*`)
- Signal: Markdown → text style ranges
- Slack: mrkdwn in blocks
- Discord: native Markdown (no conversion)

### Differences

| Step | octos | OpenClaw |
|------|---------|----------|
| **Crash recovery** | None | Write-ahead queue |
| **Send hooks** | None | Pre/post-send hooks |
| **Markdown conversion** | Telegram only | All platforms |
| **Rich payloads** | Via metadata JSON (Telegram inline keyboards only) | Dedicated `sendPayload()` with channel data |
| **Chunking** | Paragraph-aware, 50 chunk limit | Platform-specific, markdown-aware |
| **Active session routing** | Outbound forwarder checks active topic | Direct delivery |

## 5. Feature Matrix

### Telegram

| Feature | octos | OpenClaw |
|---------|---------|----------|
| Long polling | Yes (teloxide) | Yes (grammY) |
| Text messages | Yes | Yes |
| Photo/video/voice | Yes (download + send) | Yes |
| Inline keyboards | Yes (via metadata JSON) | Yes (via channelData) |
| Callback queries | Yes (routed as messages) | Yes (action handler) |
| Message editing | Yes | Yes |
| Message deletion | Yes | Yes |
| Typing indicator | Yes (ChatAction::Typing) | Yes (sendChatAction) |
| Voice indicator | Yes (ChatAction::RecordVoice) | No |
| Bot commands | Yes (/new, /s, /sessions, /back, /delete) | Yes (BotCommand API) |
| Forum topics | No | Yes |
| Stickers | No | Yes |
| Markdown → HTML | Yes (custom converter) | Yes |
| Reconnect backoff | Yes (5s base, 60s max) | Yes |
| Send-with-ID | Yes | Yes |

### WhatsApp

| Feature | octos | OpenClaw |
|---------|---------|----------|
| Protocol | Baileys via Node.js bridge | Baileys direct |
| Separate process | Yes (bridge.js sidecar) | No (same process) |
| Text messages | Yes | Yes |
| Media send/receive | Yes (via bridge HTTP port 3002) | Yes |
| Typing indicator | Yes (WebSocket {"type":"typing"}) | Yes |
| QR login | Yes (bridge handles) | Yes |
| Group support | Yes (isGroup metadata) | Yes |
| Polls | No | Yes |
| Edit/delete | No | Limited |
| Markdown conversion | No (raw text) | Yes (**→*) |

### Feishu/Lark (octos only)

| Feature | octos | OpenClaw |
|---------|---------|----------|
| WebSocket mode | Yes (default) | N/A |
| Webhook mode | Yes (port 9321) | N/A |
| China/Global regions | Yes (cn/global URLs) | N/A |
| Signature verification | Yes (SHA-1, pure Rust) | N/A |
| AES-256-CBC decryption | Yes (pure Rust) | N/A |
| Token refresh | Yes (7000s TTL) | N/A |
| Rich cards | Yes (JSON structure) | N/A |
| Image upload | Yes (file_key API) | N/A |
| Message dedup | Yes (message_id cache) | N/A |
| Thread replies | Yes | N/A |

### Slack

| Feature | octos | OpenClaw |
|---------|---------|----------|
| Socket Mode | Yes | Yes |
| Text messages | Yes | Yes |
| File downloads | Yes (Bearer auth) | Yes |
| Thread replies | Yes (thread_ts, channels only) | Yes |
| Bot mention filter | Yes (strip from text) | Yes |
| Block Kit formatting | No | Yes |
| Bot identity (custom) | No | Yes (username + emoji) |
| Reactions | No | Yes |

### Discord

| Feature | octos | OpenClaw |
|---------|---------|----------|
| Gateway | Yes (Serenity) | Yes (discord.js) |
| Text messages | Yes | Yes |
| Attachments | Yes (download) | Yes |
| Send files | No (text only) | Yes |
| Threads | No | Yes |
| Embeds | No | Yes |
| Webhook identity | No | Yes |
| Server moderation | No | Yes |
| Max message length | 1900 chars | 2000 chars |

### WeCom (octos only)

| Feature | octos | OpenClaw |
|---------|---------|----------|
| Webhook callback | Yes (port 9322) | N/A |
| AES-128-CBC crypto | Yes (pure Rust) | N/A |
| Token management | Yes (auto-refresh) | N/A |
| Text/image/voice/file | Yes | N/A |
| Department targeting | Yes (toparty) | N/A |

### Twilio (octos only)

| Feature | octos | OpenClaw |
|---------|---------|----------|
| SMS/MMS | Yes | N/A |
| Webhook (port 8090) | Yes | N/A |
| HMAC-SHA1 verification | Yes (pure Rust) | N/A |
| Media send/receive | Yes (BasicAuth download) | N/A |
| Max message length | 1600 chars | N/A |

## 6. Access Control Comparison

### octos

```
Gateway Config
  → allowed_senders: ["user1", "user2"]  (per channel entry)
  → Channel.is_allowed(sender_id) → bool

Telegram: compound ID matching ("userId|username" → match either part)
Others: direct HashSet lookup
```

**One level only.** Same rule for DMs and groups. No mention-gating.

### OpenClaw

```
DM Policy (per account):
  → "allowlist" (default) / "open" / "disabled"
  → allowFrom file: .octos/channels/{channel}/{account}/allow-from

Group Policy (per group):
  → mention-gating: only respond when @mentioned
  → tool policy: which tools available in this group

Action Gates (per action):
  → owner/admin checks before moderation actions
```

**Three levels.** DM vs group separation. Mention-gating prevents noise in groups.

### Impact

octos bots in Telegram groups respond to **every message**. This is a significant UX problem — the bot becomes noisy and unusable in active groups. Mention-gating (only respond when @mentioned or replied to) is a high-priority improvement.

## 7. Media Handling Comparison

### octos

**Download flow** (per channel):
- Each channel downloads media to a temp directory
- Filename pattern: `{channel}_{timestamp}.{ext}` (e.g., `tg_1234567890.jpg`)
- Extensions mapped from MIME types or platform file info
- Telegram: 30s timeout per download
- Twilio: BasicAuth required for media URLs
- Slack: Bearer token required for private URLs

**Send flow:**
- Telegram: Detects by extension → `send_voice()` / `send_audio()` / `send_document()`
- WhatsApp: Sends file path to bridge, auto-detects type
- Others: Limited or no media sending

**Shared utilities** (`media.rs`):
```rust
download_media(client, url, headers, dest_dir, filename) → PathBuf
is_audio(path) → bool   // .ogg, .mp3, .m4a, .wav, .oga, .opus
is_image(path) → bool   // .jpg, .jpeg, .png, .gif, .webp
```

### OpenClaw

**Download flow:**
- Centralized media handling with configurable size limits (`mediaMaxMb`)
- `mediaLocalRoots` for path validation
- Both `mediaUrl` (HTTP) and data URI support

**Send flow:**
- `outbound.sendMedia(ctx)` — unified API across channels
- Automatic MIME type detection
- Per-channel size limits (WhatsApp 16MB, Telegram 50MB, Discord 8MB)

### Differences

| Aspect | octos | OpenClaw |
|--------|---------|----------|
| Download location | Per-channel temp dir | Configurable media roots |
| Size limits | None (platform-enforced) | Configurable per-account |
| Send API | Per-channel (no unified API) | Unified `sendMedia()` |
| MIME detection | Extension-based | Content-Type + extension |
| Cleanup | Manual | Auto-cleanup policies |

## 8. Error Handling

### octos

- Channel failures logged but don't crash the gateway
- Individual message send failures → log error, continue
- Channel reconnection: Telegram has backoff (5s-60s), others reconnect on restart
- No write-ahead queue — messages lost on crash during send

### OpenClaw

- Write-ahead queue for crash recovery
- Exponential backoff with cap (60s)
- Circuit breaker for cascading failures
- `collectStatusIssues()` — machine-readable problem list with suggested fixes
- Per-account restart logic

## 9. Architecture Summary

```
                    octos                          OpenClaw
                    ──────                           ────────

Abstraction     Flat trait (12 methods)          Plugin adapters (~20 slots)
Channels        6 built-in (feature-gated)      9 built-in + extensions
Multi-account   No                              Yes (first-class)
Inbound         Direct dispatch                 Normalize → dedup → coalesce
Outbound        Chunk → send                    WAL → hooks → format → send
Access control  1-level (allowlist)             3-level (DM+group+action)
Rich format     Telegram only (metadata)        Per-channel adapters
Markdown conv.  Telegram only                   All platforms
Health check    None                            Probe + audit + issues
Streaming       Status indicator + forwarder    Delta streaming + tool cards
Dedup           Feishu/Twilio only              All channels
Mention-gate    None                            Per-group configurable
Crypto          Pure Rust (no OpenSSL)          Node.js crypto module
Deployment      Single binary                   Node.js process
Language        Rust                            TypeScript
Lines of code   ~5K (all channels)              ~15K (channel system)
```

## 10. Recommended Improvements for octos

### Must Have (High Impact, Moderate Effort)

1. **Mention-gating for groups** — Only respond when @mentioned or replied to. Prevents bot noise in group chats. Add a `require_mention` config option per channel.

2. **Universal message deduplication** — Add a message ID cache (LRU, 1000 entries, 60s TTL) to the gateway dispatcher, not per-channel. Prevents duplicate processing from webhook retries.

3. **WhatsApp Markdown conversion** — Convert `**bold**` → `*bold*`, `_italic_` → `_italic_`. WhatsApp has its own formatting syntax that differs from Markdown.

### Should Have (Medium Impact)

4. **Channel health probing** — Add `async fn health_check(&self) -> Result<ChannelHealth>` to the `Channel` trait. Surface in admin dashboard.

5. **Rich payload passthrough** — Extend `OutboundMessage.metadata` to support channel-specific payloads (already partially done for Telegram inline keyboards). Document the pattern.

6. **Outbound write-ahead queue** — Persist outbound messages before sending. On crash recovery, resend unsent messages. Prevents message loss.

### Nice to Have (Lower Priority)

7. **Platform-specific Markdown for all channels** — Slack mrkdwn, Discord native, Signal text styles.

8. **Multi-account per channel type** — Allow multiple Telegram bots, etc.

9. **Send hooks** — Pre/post-send hooks for message modification and logging.
