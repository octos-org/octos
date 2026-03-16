# Slack Reference Architecture

Based on analysis of [openclaw/openclaw](https://github.com/openclaw/openclaw) Slack implementation (Mar 2026). This documents the features and patterns octos should adopt for a production-grade Slack integration.

---

## Overview

OpenClaw's Slack channel is one of the most feature-rich workplace integrations in any AI agent framework. Key highlights:

- Dual connection modes (Socket Mode + HTTP)
- Triple token architecture (bot + app + user)
- Native streaming API (not just edit-in-place)
- Block Kit structured messages
- Per-channel configuration (system prompts, tool policies, mention requirements)
- Thread participation awareness (24h cache)
- Slash command auto-generation from agent skills

---

## 1. Dual Connection Modes

### Socket Mode (recommended for most deployments)

- Real-time WebSocket connection via Slack's Socket Mode API
- **Requires**: bot token (`xoxb-`) + app token (`xapp-`)
- No public URL needed — works behind firewalls, NAT, corporate proxies
- Reconnection policy: exponential backoff, 2s initial, 30s max, 1.8x factor, 25% jitter, 12 max attempts
- Permanent auth error detection stops retrying on: `invalid_auth`, `token_revoked`, `account_inactive`, `not_authed`, `org_login_required`, `team_access_not_granted`, `missing_scope`, `invalid_token`

### HTTP Webhook Mode

- Slack POSTs events to a public URL
- **Requires**: bot token + signing secret (HMAC-SHA256 request verification)
- Body limit: 1MB max per request
- Better for serverless/edge deployments

### octos recommendation

Start with Socket Mode (simpler setup, no URL exposure). Add HTTP mode later for serverless use cases.

```rust
pub enum SlackMode {
    Socket { bot_token: String, app_token: String },
    Http { bot_token: String, signing_secret: String, webhook_path: String },
}
```

---

## 2. Triple Token Architecture

| Token | Prefix | Purpose | Required |
|-------|--------|---------|----------|
| Bot token | `xoxb-` | All bot API calls (send, react, upload, etc.) | Always |
| App token | `xapp-` | Socket Mode WebSocket connection | Socket Mode only |
| User token | `xoxp-` | User-level API access (broader scopes) | Optional |

### Resolution priority
1. Environment variables: `SLACK_BOT_TOKEN`, `SLACK_APP_TOKEN`, `SLACK_USER_TOKEN`
2. Config file: `gateway.channels[].settings.bot_token`, etc.
3. Track source for diagnostics: "env", "config", or "none"

### Scope-aware graceful degradation
If the bot lacks `chat:write.customize` scope, retry without custom identity instead of failing. Detect missing scopes at runtime and degrade gracefully.

---

## 3. Threading Model

### Three reply modes (configurable per account + chat type)

| Mode | Behavior |
|------|----------|
| `off` | Reply in main channel; only stay in thread if already replying there |
| `first` | First reply creates/enters a thread, subsequent replies go to main |
| `all` | All replies go to thread |

### Thread participation cache

Track which threads the bot has participated in. Once the bot replies in a thread, auto-respond to follow-ups without requiring @mention.

```rust
pub struct ThreadParticipationCache {
    entries: LruCache<String, Instant>,  // thread_ts -> last_participated
    ttl: Duration,                       // 24 hours
    max_entries: usize,                  // 5000
}
```

### Thread context resolution

Extract from Slack events:
- `thread_ts` — parent message timestamp (thread root)
- `ts` — this message's timestamp
- `event_ts` — event delivery timestamp
- `parent_user_id` — who started the thread
- Computed: `is_thread_reply` = `thread_ts.is_some() && thread_ts != ts`

---

## 4. Streaming — Two Engines

### Native streaming (preferred)

Slack's `chat.startStream` / `appendStream` / `stopStream` API. Words stream progressively into a message. Final result becomes a normal message.

```rust
pub struct SlackStreamHandle {
    channel_id: String,
    stream_id: String,
    thread_ts: Option<String>,
}

pub trait SlackStreamingAdapter {
    async fn start_stream(&self, channel: &str, thread_ts: Option<&str>) -> Result<SlackStreamHandle>;
    async fn append_stream(&self, handle: &SlackStreamHandle, text: &str) -> Result<()>;
    async fn stop_stream(&self, handle: SlackStreamHandle) -> Result<String>; // returns message_ts
}
```

### Draft stream (fallback)

Post a message, then repeatedly edit via `chat.update` as content grows.

- Throttle: 1s between edits (min 250ms)
- Max: 4000 chars (Slack message limit)
- Used when native streaming is unavailable or for older Slack workspaces

---

## 5. Block Kit UI

Slack's structured message format. Supports interactive components.

### Supported blocks
- `header` — large bold text
- `section` — text with optional accessory (button, image, overflow menu)
- `image` — inline image with alt text
- `video` — embedded video
- `context` — small text/image elements
- `actions` — row of interactive elements (buttons, selects, date pickers)
- `divider` — horizontal rule
- `input` — form input fields (in modals)
- `file` — attached file reference

### Limits
- Max 50 blocks per message
- Max 100 options per select menu
- Max 75 chars per option value

### Fallback text
Always generate plain-text fallback from blocks for:
- Push notifications (mobile)
- Accessibility (screen readers)
- Non-Block-Kit clients

### Modals
Full modal lifecycle: open, update, push views. Supports form inputs, validation, submission callbacks.

---

## 6. Slash Commands

### Registration
Register commands with Slack's API during setup. Commands appear in the `/` menu.

### Skill-based auto-generation
Auto-generate slash commands from agent skills:
- Agent skill "summarize" -> `/summarize` command
- Agent skill "translate" -> `/translate` command
- Argument menus built with Block Kit (buttons, selects for large option sets)

### Argument menus
For commands with complex arguments:
- Build Block Kit interactive message with buttons/selects
- External menu store for large option sets (>100 items)
- Encode tokens for round-trip argument resolution

---

## 7. Media Pipeline

### Inbound (download)
- **Domain allowlist**: Only `*.slack.com`, `*.slack-edge.com`, `*.slack-files.com`
- **Auth handling**: First request includes `Authorization: Bearer` header; CDN redirects are pre-signed (no auth needed)
- **Limits**: Max 8 files per message, 3 concurrent downloads
- **MIME override**: `slack_audio` subtype with `video/*` MIME -> treat as `audio/*`
- **Safety**: Reject HTML responses (prevents content-type confusion)

### Outbound (upload)
Three-step flow:
1. `files.getUploadURLExternal` — get presigned upload URL
2. `POST` file to presigned URL
3. `files.completeUploadExternal` — attach to channel/thread

- Max size: configurable (default 20MB)
- Supports captions and threading
- SSRF guard on upload URLs

---

## 8. Security Layers

### Workspace validation
Validate `api_app_id` and `team_id` on every inbound event. Drop events from mismatched workspaces (prevents cross-workspace injection).

### User allowlist
Match by: Slack user ID, prefixed ID (`U12345`), display name, URL slug.
- 512-entry LRU cache for normalized slugs
- Wildcard (`*`) support

### Channel allowlist
Per-channel config supporting:
- allow/deny toggle
- `requireMention` — bot must be @mentioned to respond
- `allowBots` — whether other bots can trigger this bot
- `skills` — restrict which skills are available in this channel
- `systemPrompt` — inject channel-specific system prompt

### DM policy
- `open` — accept all DMs
- `pairing` — unknown senders get a pairing code, owner approves
- `disabled` — ignore all DMs
- Group DMs: separate toggle + allowlist

### Scope detection
At runtime, detect missing OAuth scopes and degrade gracefully:
- Missing `chat:write.customize` -> retry without custom identity
- Log warning for operator to fix scope configuration

---

## 9. Reactions

### Inbound
Listen to `reaction_added` / `reaction_removed` events. Filter by item type (only handle message reactions, ignore file reactions).

### Notification modes
- `off` — no reaction notifications
- `own` — notify only when reaction is on bot's own messages
- `allowlist` — notify for reactions from specific users
- default — notify for all reactions

### Typing reaction
Add a configurable emoji (e.g., `:thinking_face:`) to the user's message while the agent processes. Remove when done. Provides visual feedback without sending a message.

---

## 10. Unique Slack Patterns Worth Adopting

### App mention race dedup
Slack fires both `message` and `app_mention` for the same @mention. Deduplicate with a 60-second seen-message cache. Track which event won dispatch; allow one retry if the message event was dropped before the app_mention arrived.

### Assistant thread status
Call `assistant.threads.setStatus` to show "is typing..." in Slack's assistant UX. Duck-type the method check for backward compatibility.

### Per-channel system prompts
Each Slack channel can inject system prompt context:
```
"You are in #engineering. Focus on technical topics.
Code snippets should use Slack mrkdwn formatting."
```

### Message deduplication debouncing
- Top-level messages: dedupe by message timestamp
- Thread messages: dedupe by thread timestamp
- DMs: dedupe by channel (preserves batching for rapid messages)

---

## 11. Markdown to mrkdwn Conversion

Slack uses its own "mrkdwn" format, not standard Markdown.

| Standard Markdown | Slack mrkdwn |
|-------------------|-------------|
| `**bold**` | `*bold*` |
| `*italic*` | `_italic_` |
| `~~strike~~` | `~strike~` |
| `` `code` `` | `` `code` `` |
| ```` ```block``` ```` | ```` ```block``` ```` |
| `[label](url)` | `<url\|label>` |

### Escaping
Escape `&`, `<`, `>` to `&amp;`, `&lt;`, `&gt;`. Preserve Slack tokens: `<@U123>`, `<#C123>`, `<mailto:>`, `<tel:>`, `<http:>`, `<https:>`, `<slack://>`.

---

## 12. Error Handling & Reconnection

### Non-recoverable errors (stop retrying)
```
account_inactive | invalid_auth | token_revoked | token_expired |
not_authed | org_login_required | team_access_not_granted |
missing_scope | cannot_find_service | invalid_token
```

### Reconnection backoff
- Initial: 2 seconds
- Max: 30 seconds
- Factor: 1.8x
- Jitter: 25%
- Max attempts: 12

### Event handler error isolation
Every event handler wraps in try/catch. A failing reaction handler does not crash the message handler.

---

## 13. Implementation Priority for octos

| Priority | Feature | Effort | Impact |
|----------|---------|--------|--------|
| P0 | Socket Mode connection | Medium | Core functionality |
| P0 | Bot token auth + `chat.postMessage` | Low | Must-have |
| P1 | Threading (3 modes + participation cache) | Medium | Major UX improvement |
| P1 | Workspace validation + user allowlist | Low | Security requirement |
| P1 | Reconnection with auth error detection | Low | Reliability |
| P2 | Streaming (native or edit-in-place) | Medium | Polish |
| P2 | Media upload/download (3-step flow) | Medium | Feature completeness |
| P2 | Reactions (typing indicator) | Low | UX polish |
| P2 | Slash commands | Medium | Discoverability |
| P3 | Block Kit messages | High | Advanced UI |
| P3 | Per-channel config + system prompts | Medium | Enterprise feature |
| P3 | DM pairing mode | Medium | Onboarding UX |
| P3 | HTTP webhook mode | Medium | Serverless support |

---

## Reference

- OpenClaw Slack source: `src/slack/` (40+ files)
- OpenClaw Slack monitor: `src/slack/monitor/` (event handlers, auth, slash commands)
- OpenClaw streaming: `src/slack/streaming.ts` (native), `src/slack/draft-stream.ts` (fallback)
- OpenClaw Block Kit: `src/slack/blocks-input.ts`, `src/slack/blocks-fallback.ts`
- Slack API docs: https://api.slack.com/
- Slack Block Kit: https://api.slack.com/block-kit
- Slack Socket Mode: https://api.slack.com/apis/socket-mode
