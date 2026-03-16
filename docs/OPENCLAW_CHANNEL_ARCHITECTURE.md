# OpenClaw Channel Architecture Review

A comprehensive analysis of OpenClaw's channel system, compared with octos's `Channel` trait, to identify patterns worth adopting and gaps to address.

## 1. Architecture Overview

OpenClaw uses a **plugin-based adapter pattern** where each channel is a collection of optional adapters. A channel declares its capabilities via a `ChannelPlugin` interface containing ~20 optional adapter slots:

```
ChannelPlugin = {
  id, meta, capabilities, defaults,

  config:      ChannelConfigAdapter       // Multi-account management (required)
  outbound:    ChannelOutboundAdapter     // Send text/media/polls
  security:    ChannelSecurityAdapter     // DM/group access control
  gateway:     ChannelGatewayAdapter      // Start/stop listeners
  messaging:   ChannelMessagingAdapter    // Target resolution
  mentions:    ChannelMentionAdapter      // @mention parsing
  streaming:   ChannelStreamingAdapter    // Progressive text delivery
  threading:   ChannelThreadingAdapter    // Reply/thread context
  groups:      ChannelGroupAdapter        // Group-specific policies
  directory:   ChannelDirectoryAdapter    // List peers/groups
  actions:     ChannelMessageActionAdapter // Agent-discoverable actions
  status:      ChannelStatusAdapter       // Health probing
  pairing:     ChannelPairingAdapter      // QR/code-based linking
  heartbeat:   ChannelHeartbeatAdapter    // Liveness checks
  agentPrompt: ChannelAgentPromptAdapter  // Per-channel system prompt additions
  agentTools:  ChannelAgentToolFactory    // Channel-specific agent tools
  ...
}
```

octos uses a **flat trait** with 12 methods. Simpler, but less extensible.

## 2. Channel Registry

### OpenClaw: 9 Built-in + Extensions

Built-in (in `src/channels/registry.ts`):
1. **telegram** — grammY framework, most popular
2. **whatsapp** — Baileys (web client, QR-based)
3. **discord** — discord.js
4. **irc** — Custom IRC client
5. **googlechat** — Google Workspace API
6. **slack** — Socket Mode API
7. **signal** — signal-cli linked device
8. **imessage** — Apple Messages (WIP)
9. **line** — LINE Messaging API

Extensions (plugin system):
- **matrix** — `@vector-im/matrix-bot-sdk` + E2EE
- **mattermost**, **nextcloud-talk**, and 30+ others

### octos: 6 Built-in (Feature-Gated)

1. **telegram** — teloxide
2. **whatsapp** — Custom WebSocket bridge (Node.js sidecar)
3. **feishu** — Lark/Feishu webhook + event API
4. **twilio** — SMS via REST API
5. **wecom** — WeCom (Enterprise WeChat) webhook
6. **api** — HTTP/SSE gateway

### Gap Analysis

| Platform | OpenClaw | octos | Notes |
|----------|----------|---------|-------|
| Telegram | Yes | Yes | Both mature |
| WhatsApp | Yes (Baileys) | Yes (bridge) | octos uses Node sidecar |
| Discord | Yes | No | Major gap for octos |
| Slack | Yes | No | Major gap for octos |
| Signal | Yes | No | |
| Feishu/Lark | No | Yes | octos advantage |
| WeCom | No | Yes | octos advantage |
| Twilio/SMS | No | Yes | octos advantage |
| Matrix | Extension | No | Recommended to add |
| IRC | Yes | No | Low priority |
| LINE | Yes | No | Regional |
| iMessage | WIP | No | Apple-only |

## 3. Multi-Account Support

### OpenClaw: First-Class

Each channel type supports N accounts natively:

```json
{
  "channels": {
    "telegram": {
      "accounts": {
        "default": { "botToken": "..." },
        "alerts": { "botToken": "..." },
        "moderation": { "botToken": "..." }
      }
    }
  }
}
```

The `ChannelConfigAdapter` provides:
- `listAccountIds()` — enumerate accounts
- `resolveAccount(accountId)` — get config for one account
- `isEnabled(account)` / `isConfigured(account)` — lifecycle
- `describeAccount(account)` — full state snapshot

Agents can target specific accounts via tool parameters.

### octos: Single Account Per Channel

Each channel entry in `config.json` is one account. Multiple accounts of the same type require multiple channel entries with different `channel_type` values or settings. No first-class multi-account abstraction.

### Recommendation

Multi-account is valuable for separating concerns (e.g., customer support bot vs. alert bot on the same Telegram). Consider adding an `account_id` field to octos channel config and routing.

## 4. Message Flow

### Inbound Pipeline (OpenClaw)

```
Platform Event
  → Normalization (platform-specific → standard format)
  → Access Control (DM policy: allowlist/open/disabled)
  → Deduplication (debounce within window)
  → Message Coalescing (batch rapid-fire messages)
  → Context Enrichment (sender identity, thread context)
  → Agent Processing
```

Key middleware:
- **Mention stripping**: Removes bot @mentions from message text
- **Media extraction**: Normalizes images/audio/video to standard URLs
- **Thread detection**: Extracts reply-to and thread IDs

### Inbound Pipeline (octos)

```
Platform Event
  → Channel.start() callback → InboundMessage
  → Gateway dispatcher (resolve session key, topic)
  → ActorRegistry.dispatch()
  → SessionActor inbox
  → Agent.process_message()
```

### Gap: octos lacks

- **Deduplication** — no debounce for duplicate webhook deliveries
- **Message coalescing** — rapid messages processed individually
- **Mention stripping** — bot @mention stays in message text

## 5. Outbound Pipeline

### OpenClaw

```
Agent Response
  → Payload Normalization (strip HTML for plain-text channels)
  → Write-Ahead Queue (crash recovery)
  → Message Sending Hooks (can modify/cancel)
  → Text Chunking (per-channel limits, paragraph-aware)
  → Platform Formatting (Markdown → platform-native)
  → Channel Send (text / media / rich payload)
  → Message Sent Hooks (transcript recording)
  → Queue Cleanup (ack on success)
```

Platform-specific formatting:
- **Telegram**: Markdown → HTML (`<b>`, `<i>`, `<code>`)
- **WhatsApp**: Markdown → WhatsApp format (`**bold**` → `*bold*`)
- **Signal**: Markdown → text styles with ranges
- **Discord**: Native Markdown (no conversion)
- **Slack**: mrkdwn format in blocks

### octos

```
Agent Response
  → session_actor sends OutboundMessage
  → ChannelManager routes to Channel
  → split_message() (paragraph/sentence-aware chunking)
  → Channel.send()
```

### Gap: octos lacks

- **Write-ahead queue** — no crash recovery for outbound messages
- **Platform-specific Markdown conversion** — sends raw text
- **Send hooks** — no pre/post-send hooks at channel level
- **Rich payload support** — no channel-specific formatting (inline keyboards, blocks)

## 6. Typing Indicators

### OpenClaw

Sophisticated lifecycle management:

```typescript
createTypingCallbacks({
  start: () => sendTyping(),       // Platform-specific API
  stop: () => stopTyping(),
  keepaliveIntervalMs: 3000,       // Re-send every 3s
  maxConsecutiveFailures: 2,       // Circuit breaker
  maxDurationMs: 60000,            // Safety TTL (60s max)
})
```

Features:
- Keepalive loop (platforms expire typing state after 5-10s)
- Circuit breaker (auto-disable after N failures)
- Safety TTL (prevent infinite typing)
- Clean stop on reply completion

### octos

```rust
// In StatusIndicator:
channel.send_typing(&chat_id).await;  // Every 5s in status loop
```

Simpler but functional. No circuit breaker or safety TTL.

## 7. Message Editing

### OpenClaw

Part of the `ChannelMessageActionAdapter` framework:

```typescript
actions.handleAction({
  action: "edit",
  params: { messageId, chatId, text }
})
```

Platform support:
- Telegram: Full edit, any time
- Discord: Within 15 min, text only
- Slack: Anytime if in thread
- WhatsApp: Within 15 min (limited)
- Signal/iMessage: No edit support

### octos

```rust
channel.edit_message(chat_id, message_id, new_content).await
```

Same concept, simpler API. Platform limitations handled by channel implementations.

## 8. Rich Formatting & Channel-Specific Features

### OpenClaw

Each channel can expose unique features:

**Telegram**:
- Inline keyboard buttons (`payload.channelData.telegram.buttons`)
- Forum topic threads
- Sticker packs
- HTML formatting mode

**Discord**:
- Webhook identity (custom username/avatar per agent)
- Embed fields
- Server moderation tools
- Thread management

**Slack**:
- Block Kit formatting
- Bot identity (username + emoji icon)
- File uploads
- App home tab

### octos

Currently no channel-specific feature exposure. All channels get the same `send()` / `edit_message()` interface. Telegram inline keyboards, Feishu cards, etc. require adding methods to the `Channel` trait or using metadata.

### Recommendation

Add an optional `send_rich()` or `send_with_metadata()` method that accepts channel-specific JSON payloads. This keeps the core trait simple while allowing platform-native features.

## 9. Access Control

### OpenClaw: Three-Level Security

1. **DM Policy** (`security.resolveDmPolicy()`):
   - `"allowlist"` — only listed user IDs (default, safest)
   - `"open"` — accept all DMs
   - `"disabled"` — reject all DMs

2. **Group Policy** (`groups.resolveToolPolicy()`):
   - Per-group tool enablement
   - Mention-gating (only respond when @mentioned)

3. **Action-Level Gates**:
   - Owner/admin checks per action
   - Platform-specific (Telegram owner, Discord guild admin)

Allowlists stored in files: `.octos/channels/{channel}/{account}/allow-from`

### octos: Single Level

```rust
channel.is_allowed(sender_id) -> bool
```

Plus gateway-level `allowed_users` config.

### Gap

octos lacks group-level policies and mention-gating. Currently, if a bot is in a Telegram group, it responds to every message.

## 10. Health Monitoring

### OpenClaw

`ChannelStatusAdapter` provides:
- `probeAccount()` — async health check (can the bot reach the API?)
- `auditAccount()` — deeper validation (permissions, webhook state)
- `collectStatusIssues()` — list of problems with suggested fixes
- `buildAccountSnapshot()` — full state for dashboard display

### octos

No equivalent. Channel failures surface as runtime errors in logs.

### Recommendation

Add a `health_check()` method to the `Channel` trait for the admin dashboard.

## 11. Streaming / Progressive Delivery

### OpenClaw

`ChannelStreamingAdapter` + delta streaming:
- 150ms throttled text deltas from LLM → channel message edits
- Tool execution cards with real-time progress
- Reading indicator (animated dots) in web UI

### octos

Just implemented: `ChannelStreamReporter` + `run_stream_forwarder()`:
- 1s throttled text deltas → channel message edits
- Tool status inline (`Running shell...` → `✓ shell`)
- Coordinates with StatusIndicator

## 12. Key Patterns Worth Adopting

### High Priority

1. **Message deduplication** — Webhook platforms can deliver duplicates. Add a message ID cache with TTL.

2. **Mention-gating for groups** — In group chats, only respond when @mentioned. Prevents the bot from responding to every message.

3. **Platform-specific Markdown conversion** — WhatsApp, Signal, etc. have different formatting rules. Currently octos sends raw text.

4. **Channel health probing** — For the admin dashboard, expose a `probe()` method.

### Medium Priority

5. **Multi-account per channel** — Useful for separating bot personalities or use cases.

6. **Rich payload passthrough** — Allow agents to send Telegram inline keyboards, Feishu cards, etc. via metadata.

7. **Write-ahead outbound queue** — Crash recovery for message delivery.

### Low Priority

8. **Channel-specific agent tools** — Let channels inject their own tools (e.g., Discord moderation).

9. **Onboarding wizards** — CLI-guided channel setup.

10. **Plugin discovery** — Dynamic channel loading from external crates.

## 13. Architectural Comparison Summary

| Aspect | OpenClaw | octos |
|--------|----------|---------|
| **Pattern** | Plugin adapters (~20 slots) | Flat trait (12 methods) |
| **Channels** | 9 built-in + extensions | 6 feature-gated |
| **Multi-account** | First-class | Not supported |
| **Inbound pipeline** | Normalize → dedup → coalesce → enrich | Direct dispatch |
| **Outbound pipeline** | WAL → hooks → chunk → format → send | Chunk → send |
| **Access control** | 3-level (DM + group + action) | 1-level (allowlist) |
| **Rich formatting** | Per-channel adapters | Raw text |
| **Health monitoring** | Probe + audit + issues | None |
| **Streaming** | Delta streaming + tool cards | Status indicator + stream forwarder |
| **Threading** | Full thread/reply context | Session topics |
| **Error recovery** | Write-ahead queue + retry | Retry at LLM level only |
| **Extensibility** | Runtime plugin loading | Compile-time features |
| **Complexity** | High (~50 files, ~15K lines) | Low (~10 files, ~3K lines) |
| **Language** | TypeScript (Node.js) | Rust (single binary) |

octos trades extensibility for simplicity and operational ease (single binary, no runtime dependencies). The flat `Channel` trait covers 90% of use cases. The remaining 10% (rich formatting, multi-account, group policies) can be added incrementally without a full plugin system.
