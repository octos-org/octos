# OpenClaw UX Design Approach

Based on comprehensive analysis of [openclaw/openclaw](https://github.com/openclaw/openclaw) (Mar 2026). Documents the full UX philosophy, patterns, and implementation details for octos to adopt.

---

## Design Philosophy

Six core principles drive every OpenClaw surface (CLI, web, mobile, chat channels):

1. **Progressive disclosure** — Simple defaults, advanced options discoverable
2. **Real-time feedback** — Never leave the user staring at nothing
3. **Actionable errors** — Every error suggests a fix
4. **Channel-appropriate** — Respect each platform's native UX idioms
5. **Accessibility-first** — ANSI-safe, keyboard-navigable, responsive to terminal width
6. **Security visibility** — Auth state, permissions, and audit status always surfaced

---

## 1. Onboarding

### First-Run Wizard

`openclaw onboard` launches a guided interactive setup:

**Two tiers:**
- **QuickStart** — Auto-configures loopback bind, token auth, default channels. 5 prompts to a working bot.
- **Manual** — Full control over port, network bind, Tailscale, auth modes, per-channel config.

**Security-first:**
- Mandatory risk acknowledgement on first run ("OpenClaw is personal-by-default")
- Recommends `openclaw security audit --deep` after setup
- Links to security docs

**Interactive prompts** (via `@clack/prompts`):
- Select onboarding mode
- Config handling: keep existing, update, or full reset
- Auth provider selection (Anthropic, OpenAI, custom)
- Model picker with search and fallback models
- Channel setup with quickstart allowlist
- Memory/search system setup

**Validation:**
- Config snapshot validated after each step
- Helpers suggest fixes: `openclaw doctor` for invalid state
- Onboarding continues only after valid state achieved

### octos adoption

octos currently uses `crew init` with basic config generation. Adopt:
- Two-tier wizard (quick vs manual)
- Security acknowledgement
- Inline validation with recovery hints
- Model picker with search

---

## 2. CLI Design

### Color Palette

Shared design tokens in `src/terminal/palette.ts`:

| Role | Color | Hex |
|------|-------|-----|
| Accent (brand) | Burnt orange | #FF5A2D |
| Success | Green | #2FBF71 |
| Warning | Amber | #FFB020 |
| Error | Red | #E23D2D |
| Muted | Gray | #8B7F77 |

Respects `NO_COLOR` env var (disables all color). Supports `FORCE_COLOR=1` override.

### Progress Indicators

`src/cli/progress.ts` — Multi-backend progress system:

| Backend | When Used | Mechanism |
|---------|-----------|-----------|
| OSC Progress | Modern terminals (iTerm2, WezTerm) | OSC 9001 protocol for native progress bar |
| Spinner | Standard TTY | `@clack/prompts` animated spinner |
| Line | Non-TTY (CI, logs) | Simple text line updates |
| Noop | Quiet mode | Silent |

Features:
- **Delayed start** (250ms debounce) — avoids flicker for fast operations
- **Throttled updates** (250ms interval) — prevents log spam
- **Label updates** — change text mid-progress
- **Indeterminate mode** — when total unknown
- **Graceful degradation** — falls back silently in non-TTY

### Tables

`src/terminal/table.ts` — ANSI-safe responsive tables:

- Never splits ANSI escape sequences during wrapping
- Terminal-width-aware (reads `process.stdout.columns`)
- Flex columns grow/shrink to fill width
- Priority-based shrinking (flex first, then non-flex)
- Unicode box drawing borders with ASCII fallback
- Multi-line cell wrapping at break chars (`/`, `-`, `_`, `.`)
- Correct width calculation for colored text and emoji

### octos adoption

octos uses `colored` + `tabled` crates. Adopt:
- Shared palette constants (define once, use everywhere)
- Progress with delayed start (avoid flicker)
- ANSI-safe table wrapping
- `NO_COLOR` / `FORCE_COLOR` env var support

---

## 3. Message Delivery UX

### Streaming (Draft Previews)

As the agent generates, users see live text updating in a single message:

| Channel | Throttle | Max Chars | Method |
|---------|----------|-----------|--------|
| Discord | 1200ms | 2000 | Edit existing message via PATCH |
| Slack | 1000ms | 4000 | Native `chat.startStream/appendStream` or edit fallback |
| Telegram | 1000ms | 4096 | Draft transport (compose area) or edit message |

Architecture (`src/channels/draft-stream-loop.ts`):
- `update(text)` accumulates in `pendingText` buffer
- Throttle window prevents API spam
- `flush()` ensures final output is delivered
- `resetPending()` clears buffer on generation boundary

**User impact:** Never staring at a blank screen. Output appears after 2-3 seconds even for 60-second tasks.

### Typing Indicators

- **Slack:** Configurable emoji reaction (e.g., `:hourglass:`) added to user's message while processing, removed when done
- **Telegram:** Typing state API calls during generation
- **WhatsApp:** Composing presence sent before replies
- **Discord:** Typing indicator via gateway

### Read Receipts

- WhatsApp: Configurable, default on. Skipped for unauthorized senders.
- Privacy-aware: never sends receipts from blocked users

### octos adoption (partial)

octos now has `ChannelStreamReporter` with progressive edit-in-place streaming:
- `stream_reporter.rs` accumulates chunks and edits a single message at a throttled rate (1000ms)
- `<think>...</think>` blocks stripped before flushing to user (`strip_think_from_buffer()` handles partial/unclosed tags)
- Status indicator (`✦ Thinking...`) deleted on first chunk arrival
- Tool status inline: `⚙ \`tool_name\`...` → `✓ \`tool_name\`` on completion

Still needed:
- Typing indicators during processing
- Per-channel throttle tuning (Discord 1200ms, Slack 1000ms, Telegram 1000ms)

---

## 4. Queue Feedback

### What Users See During Active Runs

Six queue modes with different UX:

| Mode | User Experience |
|------|----------------|
| **steer** | Messages queue silently, processed as next prompt when run completes |
| **collect** | Messages accumulated, combined into one prompt: `[Queued messages while agent was busy]` |
| **interrupt** | User's message aborts current run, starts fresh immediately |
| **followup** | Messages queue, processed sequentially after run |
| **steer-backlog** | Like steer but backlog persists across drain cycles |
| **queue** | Alias for steer |

**Queue status display:**
```
Queue: steer (depth 2 · debounce 250ms · cap 10 · drop old)
```

**Collect mode batching:**
```
---
Queued #1: Also check topic B
---
Queued #2: And compare with topic C
```

**Drop policies** (when queue exceeds `cap`):
- `old` — drop oldest, keep newest
- `new` — drop newest, keep oldest
- `summarize` (default) — summarize dropped messages and include summary

### Interrupt/Abort

Recognizes 30+ trigger words across languages:
- English: "stop", "abort", "exit", "halt", "wait", "interrupt"
- Chinese: 停止
- Japanese: やめて
- Russian: стоп
- French: arrête
- Hindi: रुको
- Arabic: توقف

On abort: kills active run, clears queue, stops subagents, processes new message immediately.

### octos adoption ✅ Implemented

octos now has full queue mode support and multilingual abort:

- **5 queue modes**: followup (default), collect, steer, interrupt, speculative
- **Runtime switching**: `/queue followup|collect|steer|interrupt|spec` slash command
- **Collect mode**: batches queued messages with `---\nQueued #N:` separators
- **Steer mode**: keeps only latest message, discards older
- **Interrupt mode**: cancels current run, processes new message immediately
- **Speculative mode**: primary agent call spawned as tokio task; inbox polled concurrently via `select!`; overflow messages exceeding patience threshold get immediate lightweight router responses while the slow call continues
- **Multilingual abort**: 30+ trigger words across 9 languages (English, Chinese, Japanese, Russian, French, Spanish, Hindi, Arabic, Korean) in `octos-core/src/abort.rs`
- **Slash command**: `/queue` shows current mode, no LLM round-trip

See [OCTOS_UX_VISION.md](./OCTOS_UX_VISION.md) for full queue mode details and speculative overflow architecture.

---

## 5. Error UX

### Philosophy: Every Error Suggests a Fix

Examples from OpenClaw:

```
"Gateway auth is off or missing a token."
 → Run: openclaw doctor --generate-gateway-token

"Config invalid."
 → Run: openclaw doctor to repair, then re-run onboarding.

"Secret unavailable in this command path."
 → Resolve external secret source, then rerun doctor.

"Channel override requires admin permission in Discord guild."
 → (No fix possible — informational with context)
```

**Pattern:**
1. What went wrong (one sentence)
2. Why it matters (optional, for non-obvious cases)
3. How to fix it (exact CLI command when possible)
4. Reference link (docs URL for complex issues)

### Doctor Command

`openclaw doctor` — proactive health scanner:

- Scans 30+ configuration health issues
- Suggests repairs with auto-fix where safe:
  - Missing gateway mode → suggest local/remote
  - Ambiguous auth → prompt to choose
  - Deprecated profiles → offer cleanup
  - Missing token → auto-generate with consent
- `--non-interactive` for CI
- Probes gateway reachability
- Validates auth profiles via keychain
- Checks workspace integrity

### octos adoption

octos uses `eyre` with suggestion hints (good foundation). Add:
- `crew doctor` command for proactive scanning
- Inline fix commands in error messages
- Structured error categories (config, auth, channel, network)

---

## 6. Pairing / New Contact Flow

When an unauthorized user DMs the bot:

**User sees:**
```
OpenClaw: access not configured.
Sender ID: 1234567890
Pairing code: ABC123XYZ

Ask the bot owner to approve with:
openclaw pairing approve telegram ABC123XYZ
```

**Owner flow:**
```bash
openclaw pairing list                        # See pending
openclaw pairing approve telegram ABC123XYZ  # Approve
```

**Properties:**
- No credentials exchanged in user-facing flow
- Simple alphanumeric codes (8 chars, time-limited)
- Owner stays in control
- Works across all channels (generic flow)
- Auto-adds approved sender to allowlist

### octos adoption

Replace binary `allowed_senders` with pairing mode. See [OPENCLAW_CROSS_POLLINATION.md](./OPENCLAW_CROSS_POLLINATION.md) section 3.

---

## 7. Status & Health Display

### Channel Status

```bash
openclaw channels status          # Quick overview
openclaw channels status --probe  # Deep probes
openclaw channels status --json   # Machine-readable
```

**Per-account display:**
```
telegram/main: enabled · linked · running · connected
  Bot: @MyBot · Mode: webhook · DM: pairing
  Last in: 5m ago · Last out: 2m ago
  Audit: ok · Token: env

discord/prod: enabled · configured · running · connected
  Bot: MyBot#1234 · Mode: gateway · DM: allowlist
  Last in: 12m ago · Last out: 8m ago
  Audit: ok · Token: config
```

**Warning section:**
```
⚠ whatsapp/personal: not linked (run: openclaw channels login whatsapp)
⚠ slack/work: missing app token (required for Socket Mode)
```

### octos adoption

octos has basic `crew status`. Add:
- Per-account detail display
- Last in/out timestamps
- Actionable warnings with fix commands
- `--probe` for deep health checks
- `--json` for scripting

---

## 8. Command System

### Discovery

```
/help              → Common commands (short)
/commands          → Full paginated list
/commands 2        → Page 2 (Telegram pagination)
```

### Categories

| Category | Commands | Description |
|----------|----------|-------------|
| Session | `/new`, `/reset`, `/compact`, `/stop` | Conversation lifecycle |
| Options | `/think`, `/model`, `/verbose`, `/config` | Per-session toggles |
| Status | `/status`, `/whoami`, `/context` | Inspection |
| Management | Admin tools | Gateway/channel ops |
| Media | Image/video handling | Media pipeline |
| Tools | User-facing tools | Available agent tools |
| Plugins | Plugin-provided | Extension commands |

### Platform Integration

- **Discord:** Slash commands with autocomplete
- **Telegram:** Bot commands in menu
- **Slack:** Slash commands with Block Kit argument menus
- **Text:** All commands also work as plain text (`/new`, `new`, `/start`)

### octos adoption (partial)

octos has `/new`, `/s` (switch session) and now also:
- **`/adaptive`** — view/toggle adaptive routing mode (off/hedge/lane) and QoS ranking ✅
- **`/queue`** — view/change queue mode (followup/collect/steer/interrupt/speculative) ✅
- **`/stop`** — multilingual abort triggers (30+ words, 9 languages) ✅
- **Telegram command menu** — `/adaptive` and `/queue` registered in `set_my_commands()` so they appear in Telegram's bot command menu ✅

Still needed:
- `/help` with categorized command list
- `/model` with search picker
- `/think` level toggling
- `/status` for session inspection

---

## 9. Multi-Channel Consistency

### Unified Session Model

Users feel like they're talking to the same agent regardless of channel:

- Session key: `<channel>:<chatType>:<peerId>`
- Same conversation history accessible from any channel
- Model selection, thinking level, verbose mode persist
- User can start on Telegram, continue on Discord

### Channel-Appropriate Output

Same content, different formatting:

| Aspect | Discord | Telegram | Slack | WhatsApp |
|--------|---------|----------|-------|----------|
| Bold | `**text**` | `**text**` | `*text*` | `*text*` |
| Code | `` `code` `` | `` `code` `` | `` `code` `` | `` ```code``` `` |
| Links | `[label](url)` | `[label](url)` | `<url\|label>` | Plain URL |
| Max chars | 2000 | 4096 | 4000 | 4000 |
| Streaming | Edit message | Draft/edit | Native stream | None |

### octos adoption

octos's message coalescing already handles per-channel limits. Add:
- Per-channel markdown conversion (especially Slack mrkdwn)
- Unified session identity across channels

---

## 10. Accessibility

### Terminal Accessibility

- **`NO_COLOR=1`** — Disables all ANSI colors
- **`FORCE_COLOR=1`** — Forces color even in non-TTY
- **ANSI-safe width** — Correctly measures strings with escape codes
- **Terminal width responsive** — Tables/output adapt to narrow terminals
- **Screen reader safe** — Semantic text, no decorative characters in critical output
- **Keyboard navigation** — Arrow keys, Enter, Escape in all prompts
- **Multi-byte safe** — Emoji and CJK characters measured correctly

### Web Accessibility

- Control UI lazy-loads language files
- Auto-detects browser locale
- Language picker available
- Falls back to English for missing translations

### octos adoption

octos uses `colored` which respects `NO_COLOR`. Ensure:
- Table output accounts for ANSI width
- Terminal width checked before rendering
- Keyboard navigation in interactive prompts (`rustyline` already supports this)

---

## 11. Web / Control UI

### Architecture

Single-page app served on gateway port (`http://127.0.0.1:18789/`):

**Sections:**
1. **Chat** — Send messages, stream tool output, live events
2. **Channels** — Status, QR login, per-channel config
3. **Sessions** — List, pause, adjust think/verbose per session
4. **Cron** — Create, edit, run, history with delivery modes
5. **Skills** — Install, enable/disable, update API keys
6. **Nodes** — Remote compute management
7. **Exec Approvals** — Allowlists, permission policy
8. **Config** — Edit `openclaw.json` with schema validation
9. **Logs** — Live tail with filtering and export
10. **Update** — Check and install updates

**Security:**
- Device pairing on first connection
- Token/password auth
- Optimistic lock on config writes (base-hash guard)
- Loopback auto-approved, remote requires pairing

### octos adoption

octos has a React dashboard. Ensure feature parity on:
- Channel status with QR login
- Session management (list, inspect, adjust)
- Live log tailing
- Config editing with validation

---

## 12. Mobile (iOS / macOS)

### iOS App

- Live Activities on lock screen (session status)
- Voice wake word support
- Native SwiftUI chat interface
- Push notifications for incoming messages
- Background refresh for reachability
- Offline message queue

### macOS App

- Menu bar presence (always accessible)
- Voice wake ("Hey OpenClaw")
- Canvas windows for complex tasks
- Gateway auto-start via launchd
- Built-in update flow
- Settings panels for channels, skills, cron

### octos adoption

Not directly applicable (octos is CLI-first), but consider:
- System tray integration for gateway mode
- Desktop notifications for channel events

---

## 13. Notification / Alert Patterns

### Per-Channel Feedback

| Channel | While Processing | On Complete |
|---------|-----------------|-------------|
| Discord | Typing indicator + emoji reaction | Remove reaction, send reply |
| Telegram | Typing state | Send reply |
| Slack | Typing reaction emoji | Remove emoji, stream/send reply |
| WhatsApp | Composing presence | Send reply + read receipt |

### Delivery Queue

Persistent outbound queue (`src/infra/outbound/delivery-queue.ts`):
- Messages saved to disk before send attempt
- Exponential backoff: 5s → 25s → 2min → 10min
- Max 5 retries before moving to `failed/` directory
- Transparent recovery on gateway restart

---

## Summary: What octos Should Adopt (Priority Order)

| Priority | Feature | Status | Impact |
|----------|---------|--------|--------|
| P0 | Edit-in-place streaming (Discord/Slack/Telegram) | ❌ Planned | Major UX improvement |
| P0 | Actionable error messages with fix commands | ❌ Planned | Developer trust |
| P1 | Queue modes (followup/collect/steer/interrupt/speculative) | ✅ **Done** | Handles concurrent messages gracefully |
| P1 | Multilingual abort triggers (30+ words, 9 languages) | ✅ **Done** | International user support |
| P1 | Adaptive routing (Off/Hedge/Lane + QoS) | ✅ **Done** | Unique octos differentiator |
| P1 | Slash commands (/adaptive, /queue) | ✅ **Done** | Runtime control without restart |
| P1 | Auto-escalation (ResponsivenessObserver) | ✅ **Done** | Self-healing on degradation |
| P1 | Hedged racing (race 2, take winner) | ✅ **Done** | Halves worst-case latency |
| P1 | Concurrent speculative overflow | ✅ **Done** | Never stuck waiting (truly concurrent via tokio::spawn + select!) |
| P1 | `crew doctor` health scanner | ❌ Planned | Proactive issue detection |
| P1 | Typing indicators per channel | ❌ Planned | "Agent is thinking" feedback |
| P2 | Pairing mode for new contacts | ❌ Planned | Better than editing config files |
| P2 | Channel status with last in/out timestamps | ❌ Planned | Operational visibility |
| P2 | Two-tier onboarding wizard | ❌ Planned | Better first-run experience |
| P3 | Color palette system with NO_COLOR support | ❌ Planned | Consistency + accessibility |
| P3 | Delayed-start progress indicators | ❌ Planned | Avoid flicker |
| P3 | ANSI-safe table wrapping | ❌ Planned | Terminal accessibility |

---

## Related Documents

- [OPENCLAW_CROSS_POLLINATION.md](./OPENCLAW_CROSS_POLLINATION.md) — Full technical cross-pollination guide
- [CHANNEL_ADAPTER_PATTERN.md](./CHANNEL_ADAPTER_PATTERN.md) — Channel adapter trait proposal
- [SLACK_REFERENCE_ARCHITECTURE.md](./SLACK_REFERENCE_ARCHITECTURE.md) — Slack feature reference
- [PROVIDER_RACING.md](./PROVIDER_RACING.md) — Provider racing design
