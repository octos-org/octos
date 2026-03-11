# Matrix Bridge Hub Analysis for crew-rs

## Executive Summary

This document evaluates using a Matrix homeserver as a unified messaging bridge hub for crew-rs, where all external messaging platforms (Telegram, WhatsApp, Discord, etc.) connect to crew-rs through Matrix bridges instead of direct API integrations.

**Conclusion**: Matrix-as-hub is not recommended as a replacement for crew-rs's existing direct integrations, but is valuable as an **optional additional channel** for niche platforms and users who already run Matrix infrastructure.

## Background

### Current crew-rs Architecture (Direct Integration)

```
Telegram ──→ TelegramChannel  ──→┐
WhatsApp ──→ WhatsAppChannel  ──→┤
Feishu   ──→ FeishuChannel    ──→┤──→ Gateway ──→ SessionActor ──→ Agent
Twilio   ──→ TwilioChannel    ──→┤
WeCom    ──→ WeComChannel     ──→┘
```

Each platform has a dedicated Rust channel adapter implementing the `Channel` trait. Full access to platform-native APIs (inline keyboards, message editing, typing indicators, rich cards).

### Proposed Matrix Hub Architecture

```
Telegram ──→ mautrix-telegram ──→┐
WhatsApp ──→ mautrix-whatsapp ──→┤
Discord  ──→ mautrix-discord  ──→┤──→ Synapse ──→ MatrixChannel ──→ Gateway
Signal   ──→ mautrix-signal   ──→┤
Slack    ──→ mautrix-slack    ──→┘
```

crew-rs would implement a single Matrix channel adapter. All external platforms connect through Matrix bridges (mautrix family). Each external user appears as a ghost user (e.g., `@telegram_12345:server`).

## Matrix Bridge Ecosystem

### Production-Ready Bridges (mautrix family)

| Platform | Bridge | Status | Notes |
|----------|--------|--------|-------|
| Telegram | mautrix-telegram | Stable | Full-featured, well-maintained |
| WhatsApp | mautrix-whatsapp | Stable | Multi-device API, may disconnect periodically |
| Discord | mautrix-discord | Stable | Newer, more complete than matrix-appservice-discord |
| Signal | mautrix-signal | Stable | History backfill supported |
| Slack | mautrix-slack | Stable | Workspace-level integration |
| Facebook Messenger | mautrix-meta | Stable | Shares codebase with Instagram bridge |
| Instagram | mautrix-meta | Stable | DMs only |
| Google Chat | mautrix-googlechat | Functional | Less actively maintained |
| iMessage | mautrix-imessage | Functional | Requires macOS host |
| IRC | mautrix-irc (bridgev2) | Stable | Legacy protocol |

### Experimental/Community Bridges

| Platform | Status | Notes |
|----------|--------|-------|
| WeChat | Experimental | Two implementations, neither production-grade |
| LINE | Experimental | Community-maintained |
| KakaoTalk | Experimental | Community-maintained |
| LinkedIn | Newer | mautrix-linkedin, limited |

The mautrix bridges are maintained by Tulir Asokan and battle-tested at scale by Beeper (acquired by Automattic in 2024), which runs them commercially for thousands of paying users.

## How Bridging Works

### Message Flow

1. External user sends message on their platform (e.g., Telegram)
2. Bridge process (mautrix-telegram) receives via platform API
3. Bridge creates a "ghost user" on Matrix (e.g., `@telegram_12345:yourserver.com`)
4. Bridge forwards message to a "portal room" on the Matrix homeserver
5. crew-rs's Matrix client receives the message via `/sync` API
6. crew-rs processes and replies in the Matrix room
7. Bridge picks up the reply and sends it back to the original platform

### Identifying Source Platform

Ghost user MXIDs encode the platform:
- `@telegram_{userid}:server` — Telegram
- `@whatsapp_{phone}:server` — WhatsApp
- `@discord_{userid}:server` — Discord
- `@signal_{uuid}:server` — Signal
- `@slack_{userid}:server` — Slack

### What Bridges Preserve

- Text messages (all platforms)
- Images, videos, files, voice messages (most platforms)
- Emoji reactions (bidirectional on most platforms)
- Message editing (Telegram, Discord, Slack)
- Reply/quote threading (most platforms)
- Typing indicators (with MSC2409)
- Read receipts (partial)

### What Bridges Lose

- **Telegram inline keyboards / bot buttons** — flattened or dropped
- **Platform-specific rich cards** — Slack Block Kit, Discord embeds, Feishu interactive cards
- **Bot-specific APIs** — Telegram Bot API commands, Discord slash commands
- **Voice/video calls** — not bridged
- **WhatsApp business templates** — not available
- **Platform-native formatting nuances** — simplified to basic HTML/Markdown

## Comparison

| Factor | Matrix Hub | Direct Integration (current) |
|--------|-----------|------------------------------|
| **Platforms supported** | 10+ with config-only changes | Each needs a Rust channel adapter |
| **Adding new platform** | Deploy a Docker container | Write Rust code (hours to days) |
| **Inline keyboards/buttons** | Lost | Full native support |
| **Voice messages** | Basic bridging | Full native support |
| **Typing indicators** | Bridged (delayed) | Direct, immediate |
| **Message editing** | Supported (most platforms) | Full native support |
| **Latency** | +50-200ms per hop | Direct connection |
| **Operational complexity** | Synapse + Postgres + N bridges (6+ services) | Single binary |
| **Reliability** | More failure points; WhatsApp/Signal disconnect periodically | Direct, fewer moving parts |
| **Bot-specific features** | Invisible through bridges | Full access |
| **Resource usage** | ~2 GB RAM for Synapse + bridges | Included in crew binary |
| **E2EE support** | Optional (adds complexity) | Platform-dependent |

## Infrastructure Requirements

### Self-Hosted Matrix Setup

**Homeserver**: Synapse (Python, 85% market share, best bridge compatibility)
- Alternatives: Dendrite (Go, maintenance mode), Conduit (Rust, beta)
- Synapse recommended for bridge compatibility

**Database**: PostgreSQL (required for production Synapse)

**Resource estimate** (single-user AI agent hub):
- RAM: ~1-2 GB (Synapse ~500 MB, bridges ~100 MB each)
- Disk: ~10 GB
- CPU: Minimal for low-traffic use

### Docker Compose Reference

```yaml
services:
  synapse:
    image: matrixdotorg/synapse:latest
    volumes: ["./synapse-data:/data"]
    ports: ["8008:8008"]

  mautrix-telegram:
    image: dock.mau.dev/mautrix/telegram:latest
    volumes: ["./mautrix-telegram:/data"]

  mautrix-whatsapp:
    image: dock.mau.dev/mautrix/whatsapp:latest
    volumes: ["./mautrix-whatsapp:/data"]

  mautrix-discord:
    image: dock.mau.dev/mautrix/discord:latest
    volumes: ["./mautrix-discord:/data"]

  mautrix-signal:
    image: dock.mau.dev/mautrix/signal:latest
    volumes: ["./mautrix-signal:/data"]

  postgres:
    image: postgres:15
    volumes: ["./postgres-data:/var/lib/postgresql/data"]
    environment:
      POSTGRES_PASSWORD: synapse
```

Each bridge generates a registration YAML that must be added to Synapse's `homeserver.yaml` under `app_service_config_files`.

## Recommendation

### Do NOT Replace Existing Direct Integrations

crew-rs already has working, full-featured adapters for Telegram, WhatsApp, Feishu, Twilio, and WeCom. Switching to Matrix bridges would:

1. **Lose platform-specific features** (inline keyboards, rich cards, bot APIs)
2. **Add operational complexity** (6+ services to maintain instead of one binary)
3. **Increase latency** (+50-200ms per message)
4. **Reduce reliability** (more failure points, bridge disconnections)

### DO Add Matrix as an Optional Channel

Implementing a `MatrixChannel` adapter for crew-rs provides two benefits:

1. **Users with existing Matrix infrastructure** can connect to crew through their homeserver — bridges they already run will "just work"
2. **Niche platforms** (LINE, KakaoTalk, Google Chat, IRC) can be accessed through community bridges without writing Rust code

### Implementation Approach

Add a `MatrixChannel` implementing the `Channel` trait using the `matrix-sdk` Rust crate:
- Connect to a configurable homeserver with access token auth
- Map Matrix rooms to sessions (room ID → session key)
- Parse ghost user MXIDs to identify source platform
- Support text, media, reactions, message editing
- Optional E2EE via `matrix-sdk-crypto`

Configuration in `config.json`:
```json
{
  "channels": [{
    "channel_type": "matrix",
    "settings": {
      "homeserver": "https://matrix.example.com",
      "user_id": "@crew:example.com",
      "access_token": "syt_...",
      "allowed_rooms": ["!roomid:example.com"]
    }
  }]
}
```

## OpenClaw Reference

OpenClaw implements Matrix as a standalone channel at `extensions/matrix/`, using `@vector-im/matrix-bot-sdk` (Node.js). It is NOT used as a bridge hub — each channel (Telegram, Discord, Slack, Matrix, etc.) has its own independent adapter. Matrix is a peer channel, not a central router.

## Further Reading

- [Matrix.org Bridges](https://matrix.org/ecosystem/bridges/)
- [mautrix Bridge Documentation](https://docs.mau.fi/bridges/)
- [matrix-rust-sdk](https://github.com/matrix-org/matrix-rust-sdk)
- [Synapse Documentation](https://element-hq.github.io/synapse/latest/)
- [mautrix bridgev2 Framework](https://docs.mau.fi/bridges/general/bridgev2/)
