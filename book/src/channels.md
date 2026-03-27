# Gateway & Channels

Octos runs as a **gateway** that bridges messaging platforms to your LLM agent. Each platform connection is called a **channel**. You can run multiple channels simultaneously -- for example, Telegram and Slack in the same gateway process.

## Channel Overview

Channels are configured in the `gateway.channels` array of your `config.json`. Each entry specifies a `type`, optional `allowed_senders` for access control, and platform-specific `settings`.

Check which channels are compiled and configured:

```bash
octos channels status
```

This shows a table with each channel's compile status (feature flags) and config summary (environment variables set or missing).

---

## Telegram

Requires a bot token from [@BotFather](https://t.me/BotFather).

```bash
export TELEGRAM_BOT_TOKEN="123456:ABC..."
```

```json
{
  "type": "telegram",
  "allowed_senders": ["your_user_id"],
  "settings": {
    "token_env": "TELEGRAM_BOT_TOKEN"
  }
}
```

Telegram supports bot commands, inline keyboards, voice messages, images, and files.

---

## Slack

Requires a Socket Mode app with both a bot token and an app-level token.

```bash
export SLACK_BOT_TOKEN="xoxb-..."
export SLACK_APP_TOKEN="xapp-..."
```

```json
{
  "type": "slack",
  "settings": {
    "bot_token_env": "SLACK_BOT_TOKEN",
    "app_token_env": "SLACK_APP_TOKEN"
  }
}
```

---

## Discord

Requires a bot token from the [Discord Developer Portal](https://discord.com/developers/applications).

```bash
export DISCORD_BOT_TOKEN="..."
```

```json
{
  "type": "discord",
  "settings": {
    "token_env": "DISCORD_BOT_TOKEN"
  }
}
```

---

## WhatsApp

Requires a Node.js bridge (Baileys) running at a WebSocket URL.

```json
{
  "type": "whatsapp",
  "settings": {
    "bridge_url": "ws://localhost:3001"
  }
}
```

---

## Feishu (China)

Feishu uses WebSocket long-connection mode by default (no public URL needed).

```bash
export FEISHU_APP_ID="cli_..."
export FEISHU_APP_SECRET="..."
```

```json
{
  "type": "feishu",
  "settings": {
    "app_id_env": "FEISHU_APP_ID",
    "app_secret_env": "FEISHU_APP_SECRET"
  }
}
```

Build with the `feishu` feature flag:

```bash
cargo build --release -p octos-cli --features feishu
```

---

## Lark (International)

Larksuite (international) does **not** support WebSocket mode. Use webhook mode instead, where Lark pushes events to your server via HTTP POST.

```
Lark Cloud --> ngrok --> localhost:9321/webhook/event --> Gateway --> LLM
```

### Developer Console Setup

1. Go to [open.larksuite.com/app](https://open.larksuite.com/app) and create (or select) an app
2. Add **Bot** capability under Features
3. Configure event subscription:
   - Events & Callbacks > Event Configuration > Edit subscription method
   - Select "Send events to developer server"
   - Set request URL to `https://YOUR_NGROK_URL/webhook/event`
4. Add event: `im.message.receive_v1` (Receive Message)
5. Enable permissions: `im:message`, `im:message:send_as_bot`, `im:resource`
6. Publish the app: App Release > Version Management > Create Version > Apply for Online Release

### Config

```bash
export LARK_APP_ID="cli_..."
export LARK_APP_SECRET="..."
```

```json
{
  "type": "lark",
  "allowed_senders": [],
  "settings": {
    "app_id_env": "LARK_APP_ID",
    "app_secret_env": "LARK_APP_SECRET",
    "region": "global",
    "mode": "webhook",
    "webhook_port": 9321
  }
}
```

### Settings Reference

| Setting | Description | Default |
|---------|-------------|---------|
| `app_id_env` | Env var name for App ID | `FEISHU_APP_ID` |
| `app_secret_env` | Env var name for App Secret | `FEISHU_APP_SECRET` |
| `region` | `"cn"` (Feishu) or `"global"` / `"lark"` (Larksuite) | `"cn"` |
| `mode` | `"ws"` (WebSocket) or `"webhook"` (HTTP) | `"ws"` |
| `webhook_port` | Port for webhook HTTP server | `9321` |
| `encrypt_key` | Encrypt Key from Lark console (for AES-256-CBC) | none |
| `verification_token` | Verification Token from Lark console | none |

### Encryption (Optional)

If you configure an Encrypt Key in the Lark console (Events & Callbacks > Encryption Strategy), add it to your config:

```json
{
  "type": "lark",
  "settings": {
    "app_id_env": "LARK_APP_ID",
    "app_secret_env": "LARK_APP_SECRET",
    "region": "global",
    "mode": "webhook",
    "webhook_port": 9321,
    "encrypt_key": "your-encrypt-key-here",
    "verification_token": "your-verification-token"
  }
}
```

With encryption enabled, Lark sends encrypted POST bodies. The gateway decrypts using AES-256-CBC with SHA-256 key derivation and validates signatures via the `X-Lark-Signature` header.

### Supported Message Types

**Inbound:** text, images, files (PDF, docs), audio, video, stickers

**Outbound:** markdown (via interactive cards), image upload, file upload

### Running

```bash
# Start ngrok tunnel
ngrok http 9321

# Start gateway
LARK_APP_ID="cli_xxxxx" LARK_APP_SECRET="xxxxx" octos gateway --cwd /path/to/workdir
```

### Troubleshooting

| Issue | Solution |
|-------|----------|
| 404 on WS endpoint | Larksuite international does not support WebSocket. Use `"mode": "webhook"` |
| Challenge verification fails | Ensure ngrok is running and the URL matches the Lark console |
| No events received | Publish the app version after adding events. Check Event Log in the console |
| Bot does not reply | Verify `im:message:send_as_bot` permission is granted |
| Ngrok URL changed | Free ngrok URLs change on restart. Update the request URL in Lark console |

---

## Email (IMAP/SMTP)

Polls an IMAP inbox for inbound messages and replies via SMTP. Feature-gated behind `email`.

```bash
export EMAIL_USERNAME="bot@example.com"
export EMAIL_PASSWORD="app-specific-password"
```

```json
{
  "type": "email",
  "allowed_senders": ["trusted@example.com"],
  "settings": {
    "imap_host": "imap.gmail.com",
    "imap_port": 993,
    "smtp_host": "smtp.gmail.com",
    "smtp_port": 465,
    "username_env": "EMAIL_USERNAME",
    "password_env": "EMAIL_PASSWORD",
    "from_address": "bot@example.com",
    "poll_interval_secs": 30,
    "max_body_chars": 10000
  }
}
```

---

## WeCom (WeChat Work)

Requires a Custom App with a message callback URL. Feature-gated behind `wecom`.

```bash
export WECOM_CORP_ID="ww..."
export WECOM_AGENT_SECRET="..."
```

```json
{
  "type": "wecom",
  "settings": {
    "corp_id_env": "WECOM_CORP_ID",
    "agent_secret_env": "WECOM_AGENT_SECRET",
    "agent_id": "1000002",
    "verification_token": "...",
    "encoding_aes_key": "...",
    "webhook_port": 9322
  }
}
```

---

## WeChat (via WorkBuddy Bridge)

Regular WeChat users can connect to your agent through a WorkBuddy desktop bridge. WorkBuddy handles the WeChat transport; Octos handles the AI logic via its WeCom Bot channel.

```
WeChat (mobile) --> WorkBuddy (desktop) --> WeCom group robot (WSS) --> octos wecom-bot channel
```

### Setup

1. Create a **WeCom group robot** in the [WeCom Admin Console](https://work.weixin.qq.com/) under Applications > Group Robot. Note the Bot ID and Secret.

2. Configure the `wecom-bot` channel:

```bash
export WECOM_BOT_SECRET="your_robot_secret_here"
```

```json
{
  "type": "wecom-bot",
  "allowed_senders": [],
  "settings": {
    "bot_id": "YOUR_BOT_ID",
    "secret_env": "WECOM_BOT_SECRET"
  }
}
```

3. Build and start:

```bash
cargo build --release -p octos-cli --features "wecom-bot"
octos gateway
```

4. Install the **WorkBuddy** desktop client, link it to your WeChat via QR scan, and connect it to the same WeCom group robot.

### Connection Details

| Property | Value |
|----------|-------|
| Protocol | WebSocket (WSS) |
| Endpoint | `wss://openws.work.weixin.qq.com` |
| Heartbeat | Ping/pong every 30 seconds |
| Auto-reconnect | Yes, exponential backoff (5s--60s) |
| Max message length | 4096 characters |
| Message format | Markdown |

The `wecom-bot` channel uses an outbound WebSocket connection -- no public URL or port forwarding is required. This makes it suitable for servers behind NAT or firewalls.

### Limitations

- **Text only** -- voice and image messages are passed as placeholders
- **No message editing** -- responses are sent as new messages
- **One direction** -- WeChat-to-Octos is automatic; for proactive messages, use cron jobs

---

## Session Control Commands

In any gateway channel, the following commands manage conversation sessions:

| Command | Description |
|---------|-------------|
| `/new` | Create a new session (forks the last 10 messages from the current conversation) |
| `/new <name>` | Create a named session |
| `/s <name>` | Switch to a named session |
| `/s` | Switch to the default session |
| `/sessions` | List all sessions for this chat |
| `/back` | Switch to the previously active session |
| `/delete` | Delete the current session |

Only one session is **active** at a time per chat. Messages are routed to the active session. Inactive sessions can still run background tasks (deep search, pipelines, etc.). When an inactive session finishes work, you receive a notification -- use `/s <name>` to view the results.

---

## Voice Transcription

Voice and audio messages from channels are automatically transcribed before being sent to the agent. The system tries local ASR first (via the OminiX engine) and falls back to cloud-based Whisper when local ASR is unavailable. The transcription is prepended as `[transcription: ...]`.

```bash
# Local ASR (preferred) -- set automatically by octos serve
export OMINIX_API_URL="http://localhost:8080"

# Cloud fallback
export GROQ_API_KEY="gsk_..."
```

Voice configuration in `config.json`:

```json
{
  "voice": {
    "auto_asr": true,
    "auto_tts": true,
    "default_voice": "vivian",
    "asr_language": null
  }
}
```

- **`auto_asr`** -- automatically transcribe incoming voice/audio messages
- **`auto_tts`** -- automatically synthesize voice replies when the user sends voice
- **`default_voice`** -- voice preset for auto-TTS
- **`asr_language`** -- force a specific language for transcription (`null` = auto-detect)

---

## Access Control

Use `allowed_senders` to restrict who can interact with the agent. An empty list allows everyone.

```json
{
  "type": "telegram",
  "allowed_senders": ["123456", "789012"]
}
```

Each channel type uses its own sender identifier format (Telegram user IDs, email addresses, WeCom user IDs, etc.).

---

## Cron Jobs

The agent can schedule recurring tasks that deliver messages through any channel:

```bash
octos cron list                          # List active jobs
octos cron list --all                    # Include disabled jobs
octos cron add --name "report" --message "Generate daily report" --cron "0 0 9 * * * *"
octos cron add --name "check" --message "Check status" --every 3600
octos cron add --name "once" --message "Run migration" --at "2025-03-01T09:00:00Z"
octos cron remove <job-id>
octos cron enable <job-id>               # Enable a job
octos cron enable <job-id> --disable     # Disable a job
```

Jobs support an optional `timezone` field with IANA timezone names (e.g., `"America/New_York"`, `"Asia/Shanghai"`). When omitted, UTC is used.

---

## Message Coalescing

Long responses are automatically split into channel-safe chunks:

| Channel | Max chars per message |
|---------|-----------------------|
| Telegram | 4000 |
| Discord | 1900 |
| Slack | 3900 |

Split preference: paragraph boundary > newline > sentence end > space > hard cut.

---

## Config Hot-Reload

The gateway detects config file changes automatically:

- **Hot-reloaded** (no restart): system prompt, AGENTS.md, SOUL.md, USER.md
- **Restart required**: provider, model, API keys, channel settings

Changes are detected via SHA-256 hashing with debounce.
