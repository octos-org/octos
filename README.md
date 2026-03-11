# crew-rs

Rust-native AI agent framework with multi-channel gateway, 14 LLM providers, web dashboard, and coding automation tools.

## Documentation / 文档

**[User Guide (English)](docs/user-guide.md)** | **[用户指南 (中文)](docs/user-guide-zh.md)**

Comprehensive guides covering dashboard setup, LLM providers, tool configuration, profile management, bundled skills, platform skills (ASR/TTS), custom skill development, and more.

---

## Table of Contents

- [Features](#features)
- [Installation](#installation)
  - [Prerequisites](#prerequisites)
  - [Build from Source](#build-from-source)
  - [Feature Flags](#feature-flags)
- [Quick Start](#quick-start)
- [CLI Commands](#cli-commands)
- [Configuration](#configuration)
  - [Config File Locations](#config-file-locations)
  - [Basic Config](#basic-config)
  - [Gateway Config](#gateway-config)
  - [Full Config Reference](#full-config-reference)
- [LLM Providers](#llm-providers)
- [Adding Messaging Apps](#adding-messaging-apps)
  - [Telegram](#telegram)
  - [Discord](#discord)
  - [Slack](#slack)
  - [WhatsApp](#whatsapp)
  - [Feishu / Lark](#feishu--lark)
  - [Email (IMAP/SMTP)](#email-imapsmtp)
  - [Twilio SMS](#twilio-sms)
  - [WeCom / WeChat Work](#wecom--wechat-work)
- [Web Dashboard](#web-dashboard)
  - [Starting the Dashboard](#starting-the-dashboard)
  - [Email OTP Authentication](#email-otp-authentication)
  - [User Management](#user-management)
  - [Profile Management](#profile-management)
  - [Dashboard API Reference](#dashboard-api-reference)
- [Multi-User Setup](#multi-user-setup)
- [Tools](#tools)
- [Account Management](#account-management)
- [Office Tools](#office-tools)
- [Memory System](#memory-system)
- [Skills System](#skills-system)
- [Sandbox Isolation](#sandbox-isolation)
- [Hooks](#hooks)
- [Cron & Heartbeat](#cron--heartbeat)
- [Architecture](#architecture)
- [Development](#development)
- [License](#license)

---

## Features

- **14 LLM providers**: Anthropic, OpenAI, Gemini, OpenRouter, DeepSeek, Groq, Moonshot/Kimi, DashScope/Qwen, MiniMax, Zhipu/GLM, Z.AI, Nvidia NIM, Ollama, vLLM
- **Multi-channel gateway**: CLI, Telegram, Discord, Slack, WhatsApp, Feishu/Lark, Email (IMAP/SMTP), Twilio SMS, WeCom/WeChat Work
- **Web dashboard**: Multi-user admin panel with per-user profile management, gateway controls, and live log streaming
- **Email OTP auth**: Larksuite-style email verification code login for the dashboard
- **OAuth login**: `crew auth login` with PKCE browser flow, device code flow, or paste-token
- **Provider failover**: Automatic fallback chain across multiple LLM providers
- **Sub-provider spawning**: Configure multiple LLMs for subagent use with cost/capability metadata
- **Vision support**: Send images to vision-capable LLMs (Anthropic, OpenAI, Gemini, OpenRouter)
- **Voice transcription**: Groq Whisper auto-transcription for voice messages
- **Media handling**: Auto-download photos, voice, audio, documents from channels
- **Interactive chat**: Multi-turn conversation with readline history
- **Single-message mode**: Non-interactive `crew chat --message "..."` for scripting
- **Memory system**: Episodic memory, daily notes, long-term memory, hybrid BM25+vector search
- **Skills system**: Markdown-based skills with YAML frontmatter + 6 built-in skills
- **Sandbox isolation**: bwrap (Linux), sandbox-exec (macOS), Docker with resource limits
- **Tool policies**: Allow/deny lists, wildcard matching, named groups, provider-specific filtering
- **Context compaction**: Token-aware message summarization when context window fills
- **Config hot-reload**: SHA-256 change detection, live system prompt updates
- **Message coalescing**: Channel-aware response splitting (Telegram/Discord/Slack limits)
- **MCP integration**: JSON-RPC stdio transport for Model Context Protocol servers
- **Cron & heartbeat**: Scheduled tasks (interval, one-shot, cron expressions) and periodic background checks
- **Subagent spawning**: Background agents for long-running tasks
- **Cross-channel messaging**: Send messages across any connected channel
- **Provider auto-detect**: Automatically selects provider from model name
- **Built-in tools**: Shell, file ops, glob, grep, list_dir, web search/fetch, message, spawn, cron, browser (feature-gated)
- **Plugin system**: Load custom tools from `plugins/` directories
- **Config migration**: Versioned config with automatic migration
- **Adaptive routing**: Metrics-driven provider selection with latency tracking and circuit breakers
- **Self-updater**: Check and install updates via admin API
- **Pipeline orchestration**: DOT-based multi-step workflow execution with human gates, supervision strategies, context fidelity control, and thread reuse
- **Office tools**: DOCX/PPTX/XLSX manipulation (extract, pack, validate)
- **Prompt injection guard**: Detection and sanitization of injection attempts
- **8 bundled app-skills**: news, deep-search, deep-crawl, send-email, weather, account-manager, clock, ASR
- **Typed LLM errors**: Structured error hierarchy (rate limit, auth, context overflow) with retryability classification
- **LLM middleware**: Composable request/response interceptors (logging, cost tracking, caching)
- **Model catalog**: Programmatic model discovery with capabilities, costs, and alias-based lookup
- **High-level LLM client**: Ergonomic `generate()`, `generate_object()`, `generate_typed<T>()`, `stream()` APIs
- **Execution environments**: Abstracted local/Docker command execution with env sanitization
- **Pure Rust TLS**: No OpenSSL dependency (uses rustls)

---

## Installation

### Prerequisites

- **Rust 1.85.0+** (Edition 2024)
- At least one LLM API key (e.g., `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`)
- (Optional) Chrome/Chromium for browser automation
- (Optional) Node.js for WhatsApp bridge and pptxgenjs skill
- (Optional) `ffmpeg` for video/animation skills (mofa-pptx)
- (Optional) LibreOffice (`soffice`) and Poppler (`pdftoppm`) for office document conversion and visual QA

#### macOS (Homebrew)

```bash
# System dependencies for skills (optional but recommended)
brew install node ffmpeg poppler
brew install --cask libreoffice
npm install -g pptxgenjs react-icons react react-dom sharp
```

### Build from Source

```bash
# Clone the repository
git clone https://github.com/hagency-org/crew-rs.git
cd crew-rs

# Quick local deploy (builds, installs, sets up service)
./scripts/local-deploy.sh --full    # All features + dashboard + skills
./scripts/local-deploy.sh --minimal # CLI + chat only

# Or install manually:
cargo install --path crates/crew-cli

# Install with all messaging channels + dashboard
cargo install --path crates/crew-cli --features api,telegram,discord,slack,whatsapp,feishu,email

# Install with specific channels
cargo install --path crates/crew-cli --features telegram,discord

# Install with browser automation (requires Chrome/Chromium)
cargo install --path crates/crew-cli --features browser

# Or build locally without installing
cargo build --release
./target/release/crew --help
```

See **[Local Deployment Guide](docs/LOCAL_DEPLOYMENT.md)** for platform-specific instructions (macOS, Linux, Windows/WSL2), background service setup, and troubleshooting.

### Feature Flags

| Feature | Description |
|---------|-------------|
| `api` | Web dashboard + REST API server (`crew serve`) |
| `telegram` | Telegram bot channel (teloxide) |
| `discord` | Discord bot channel (serenity) |
| `slack` | Slack bot channel (WebSocket Socket Mode) |
| `whatsapp` | WhatsApp channel (Node.js bridge via WebSocket) |
| `feishu` | Feishu/Lark channel (WebSocket + REST) |
| `email` | Email channel (IMAP polling + SMTP sending) |
| `twilio` | Twilio SMS channel |
| `git` | Git integration tools |
| `ast` | AST parsing tools (tree-sitter) |
| `wecom` | WeCom/WeChat Work channel |
| `admin-bot` | Admin bot via Telegram (requires `api`) |

---

## Quick Start

```bash
# Initialize configuration and workspace
crew init

# Set your API key (or use OAuth login)
export ANTHROPIC_API_KEY=your-key-here
# Or: crew auth login -p anthropic

# Interactive chat
crew chat

# Single-message mode (non-interactive)
crew chat --message "Add a hello function to lib.rs"

# Check system status
crew status
```

---

## CLI Commands

### `crew chat`

Interactive multi-turn conversation:

```bash
crew chat                          # Default provider (Anthropic)
crew chat --provider openai        # Use OpenAI
crew chat --model gpt-4o           # Auto-detects OpenAI from model name
crew chat --verbose                # Show tool outputs
crew chat --message "Fix the bug"  # Single message, non-interactive
```

### `crew gateway`

Run as a persistent multi-channel messaging daemon:

```bash
crew gateway                       # Uses config from .crew/config.json
crew gateway --provider openai     # Override provider
crew gateway --verbose             # Verbose logging
crew gateway --data-dir /data/bob  # Custom data directory
```

### `crew serve`

Start the web dashboard + REST API server (requires `api` feature):

```bash
crew serve                         # Listen on 127.0.0.1:8080
crew serve --port 8090             # Custom port
crew serve --host 0.0.0.0          # Accept connections from all interfaces
crew serve --auth-token my-secret  # Set admin auth token
crew serve --data-dir ~/.crew      # Custom data directory
crew serve --config /path/to/config.json
```

### `crew init`

Initialize workspace with config and bootstrap files:

```bash
crew init              # Interactive setup
crew init --defaults   # Use defaults (Anthropic/Claude)
```

Creates:
- `.crew/config.json` - Configuration
- `.crew/AGENTS.md` - Agent instructions
- `.crew/SOUL.md` - Personality definition
- `.crew/USER.md` - User preferences
- `.crew/memory/`, `.crew/sessions/`, `.crew/skills/` directories

### `crew status`

Show system status (config, API keys, bootstrap files).

### `crew auth`

OAuth login and API key management:

```bash
crew auth login --provider openai         # PKCE browser OAuth flow
crew auth login --provider openai --device-code  # Device code flow
crew auth login --provider anthropic      # Paste-token flow
crew auth logout --provider openai        # Remove stored credential
crew auth status                          # Show authenticated providers
```

### `crew cron`

Manage scheduled cron jobs:

```bash
crew cron list                          # List active jobs
crew cron list --all                    # Include disabled jobs
crew cron add --name "daily" --message "Run report" --cron "0 0 9 * * * *"
crew cron add --name "check" --message "Check status" --every 3600
crew cron remove <job-id>
crew cron enable <job-id>               # Enable a job
crew cron enable <job-id> --disable     # Disable a job
```

### `crew skills`

Manage skills:

```bash
crew skills list                          # List installed skills
crew skills install user/repo/skill-name  # Install from GitHub
crew skills remove skill-name             # Remove a skill
```

### `crew channels status`

Show configured gateway channels and their compile/config status.

### `crew account`

Manage sub-accounts under profiles:

```bash
crew account list --profile <id>     # List sub-accounts
crew account create --profile <id> <name>  # Create sub-account
crew account update <id>             # Update sub-account
crew account delete <id>             # Delete sub-account
crew account info <id>               # Show sub-account details
crew account start <id>              # Start sub-account gateway
crew account stop <id>               # Stop sub-account gateway
```

### `crew office`

Office file manipulation:

```bash
crew office extract <file>           # Extract text from DOCX/PPTX/XLSX
crew office unpack <file> <dir>      # Unpack archive to directory
crew office pack <dir> <output>      # Pack directory into archive
crew office clean <dir>              # Remove orphaned files from unpacked PPTX
crew office add-slide <dir> <source> # Add slide by duplicating or from layout
crew office validate <path>          # Validate document structure
```

### Other Commands

```bash
crew clean [--all]       # Clean up state/database files
crew completions <shell> # Generate shell completions (bash/zsh/fish)
```

---

## Configuration

### Config File Locations

Config is loaded in this priority order:

1. **CLI flag**: `--config /path/to/config.json`
2. **Project-local**: `.crew/config.json` (in current directory)
3. **Global**: Platform-specific config directory:
   - **macOS**: `~/Library/Application Support/crew/config.json`
   - **Linux**: `~/.config/crew/config.json`
   - **Windows**: `%APPDATA%\crew\config.json`

### Basic Config

```json
{
  "provider": "anthropic",
  "model": "claude-sonnet-4-20250514",
  "api_key_env": "ANTHROPIC_API_KEY"
}
```

### Gateway Config

```json
{
  "provider": "anthropic",
  "model": "claude-sonnet-4-20250514",
  "gateway": {
    "channels": [
      { "type": "cli" },
      { "type": "telegram", "allowed_senders": ["123456789"] },
      { "type": "slack", "settings": {
        "bot_token_env": "SLACK_BOT_TOKEN",
        "app_token_env": "SLACK_APP_TOKEN"
      }}
    ],
    "max_history": 50,
    "max_sessions": 1000,
    "max_concurrent_sessions": 10,
    "system_prompt": "You are a helpful assistant.",
    "queue_mode": "followup"
  }
}
```

### Full Config Reference

```json
{
  "version": 1,
  "provider": "anthropic",
  "model": "claude-sonnet-4-20250514",
  "base_url": null,
  "api_key_env": "ANTHROPIC_API_KEY",

  "gateway": {
    "channels": [],
    "max_history": 50,
    "max_sessions": 1000,
    "max_concurrent_sessions": 10,
    "system_prompt": "Custom system prompt",
    "queue_mode": "followup"
  },

  "fallback_models": [
    { "provider": "openai", "model": "gpt-4o" },
    { "provider": "gemini", "model": "gemini-2.0-flash" }
  ],

  "sub_providers": [
    {
      "key": "cheap",
      "provider": "openai",
      "model": "gpt-4o-mini",
      "description": "Fast and cheap for simple tasks",
      "default_context_window": 8000
    },
    {
      "key": "strong",
      "provider": "anthropic",
      "model": "claude-sonnet-4-20250514",
      "description": "Best quality for complex reasoning"
    }
  ],

  "embedding": {
    "provider": "openai",
    "api_key_env": "OPENAI_API_KEY",
    "base_url": null
  },

  "sandbox": {
    "enabled": true,
    "allow_network": true
  },

  "tool_policy": {
    "allow": ["shell", "read_file", "write_file"],
    "deny": ["browser"]
  },

  "tool_policy_by_provider": {
    "gemini": { "deny": ["diff_edit"] }
  },

  "context_filter": ["code", "search"],

  "mcp_servers": [
    {
      "name": "my-server",
      "command": ["npx", "-y", "@modelcontextprotocol/server-filesystem"],
      "env": {}
    }
  ],

  "hooks": [
    {
      "event": "before_tool_call",
      "command": ["~/scripts/approve.sh"],
      "timeout_ms": 5000,
      "tool_filter": "shell"
    }
  ],

  "dashboard_auth": {
    "smtp": {
      "host": "smtp.gmail.com",
      "port": 465,
      "username": "noreply@example.com",
      "password_env": "SMTP_PASSWORD",
      "from_address": "noreply@example.com"
    },
    "session_expiry_hours": 24,
    "allow_self_registration": true
  }
}
```

Environment variables can be used in config values with `${VAR_NAME}` syntax:

```json
{
  "base_url": "${CUSTOM_API_BASE}"
}
```

---

## LLM Providers

| Provider | API Key Env | Default Model | Notes |
|----------|-------------|---------------|-------|
| `anthropic` | `ANTHROPIC_API_KEY` | claude-sonnet-4-20250514 | Default provider |
| `openai` | `OPENAI_API_KEY` | gpt-4o | |
| `gemini` | `GEMINI_API_KEY` | gemini-2.0-flash | |
| `openrouter` | `OPENROUTER_API_KEY` | anthropic/claude-sonnet-4-20250514 | Multi-model router |
| `deepseek` | `DEEPSEEK_API_KEY` | deepseek-chat | |
| `groq` | `GROQ_API_KEY` | llama-3.3-70b-versatile | Fast inference |
| `moonshot` | `MOONSHOT_API_KEY` | kimi-k2.5 | Also: `kimi` |
| `dashscope` | `DASHSCOPE_API_KEY` | qwen-max | Also: `qwen` |
| `minimax` | `MINIMAX_API_KEY` | MiniMax-Text-01 | |
| `zhipu` | `ZHIPU_API_KEY` | glm-4-plus | Also: `glm` |
| `zai` | `ZAI_API_KEY` | (varies) | Z.AI multi-model |
| `nvidia` | `NVIDIA_API_KEY` | (varies) | Also: `nim` |
| `ollama` | (none) | llama3.2 | Local models |
| `vllm` | `VLLM_API_KEY` | (requires `--model`) | Self-hosted |

Provider is auto-detected from model name when not specified (e.g., `--model gpt-4o` selects OpenAI).

**Provider failover**: Configure a fallback chain so the agent automatically tries the next provider on retriable errors:

```json
{
  "provider": "anthropic",
  "model": "claude-sonnet-4-20250514",
  "fallback_models": [
    { "provider": "openai", "model": "gpt-4o" },
    { "provider": "gemini", "model": "gemini-2.0-flash" }
  ]
}
```

---

## Adding Messaging Apps

Each channel is feature-gated. You must build with the corresponding feature flag enabled. The gateway is configured via `gateway.channels[]` in your config file. Each channel entry has a `type`, optional `allowed_senders` (empty = allow all), and channel-specific `settings`.

### Telegram

**Prerequisites**: Create a Telegram bot via [@BotFather](https://t.me/BotFather) and get the bot token.

**Build**:
```bash
cargo install --path crates/crew-cli --features telegram
```

**Config** (`.crew/config.json`):
```json
{
  "provider": "anthropic",
  "model": "claude-sonnet-4-20250514",
  "gateway": {
    "channels": [
      {
        "type": "telegram",
        "allowed_senders": ["123456789"],
        "settings": {
          "token_env": "TELEGRAM_BOT_TOKEN"
        }
      }
    ]
  }
}
```

**Environment variables**:
```bash
export ANTHROPIC_API_KEY=your-anthropic-key
export TELEGRAM_BOT_TOKEN=your-telegram-bot-token
```

**Setup steps**:
1. Message [@BotFather](https://t.me/BotFather) on Telegram and create a new bot with `/newbot`
2. Copy the bot token provided by BotFather
3. Set it as `TELEGRAM_BOT_TOKEN` environment variable
4. (Optional) Get your Telegram user ID by messaging [@userinfobot](https://t.me/userinfobot) and add it to `allowed_senders` to restrict access
5. Run `crew gateway`

**Features**: Text messages, photo/document/voice/audio download, vision (sends images to LLM), voice transcription (via Groq Whisper), message coalescing (4096 char limit).

---

### Discord

**Prerequisites**: Create a Discord application at the [Discord Developer Portal](https://discord.com/developers/applications) and get a bot token.

**Build**:
```bash
cargo install --path crates/crew-cli --features discord
```

**Config**:
```json
{
  "gateway": {
    "channels": [
      {
        "type": "discord",
        "settings": {
          "token_env": "DISCORD_BOT_TOKEN"
        }
      }
    ]
  }
}
```

**Environment variables**:
```bash
export DISCORD_BOT_TOKEN=your-discord-bot-token
```

**Setup steps**:
1. Go to [Discord Developer Portal](https://discord.com/developers/applications) and create a new application
2. Navigate to **Bot** section, click "Add Bot", and copy the token
3. Under **Privileged Gateway Intents**, enable **Message Content Intent**
4. Generate an invite URL under **OAuth2 > URL Generator**:
   - Scopes: `bot`
   - Bot Permissions: `Send Messages`, `Read Message History`, `Attach Files`
5. Invite the bot to your server using the generated URL
6. Set `DISCORD_BOT_TOKEN` and run `crew gateway`

**Features**: Text messages, attachment handling, message coalescing (2000 char limit).

---

### Slack

**Prerequisites**: Create a Slack app with Socket Mode enabled.

**Build**:
```bash
cargo install --path crates/crew-cli --features slack
```

**Config**:
```json
{
  "gateway": {
    "channels": [
      {
        "type": "slack",
        "settings": {
          "bot_token_env": "SLACK_BOT_TOKEN",
          "app_token_env": "SLACK_APP_TOKEN"
        }
      }
    ]
  }
}
```

**Environment variables**:
```bash
export SLACK_BOT_TOKEN=xoxb-your-bot-token
export SLACK_APP_TOKEN=xapp-your-app-token
```

**Setup steps**:
1. Go to [Slack API](https://api.slack.com/apps) and create a new app
2. Enable **Socket Mode** under Settings > Socket Mode (this generates the `xapp-` app-level token)
3. Under **OAuth & Permissions**, add these bot token scopes:
   - `chat:write`
   - `channels:history`
   - `groups:history`
   - `im:history`
   - `mpim:history`
4. Under **Event Subscriptions**, enable events and subscribe to:
   - `message.channels`
   - `message.groups`
   - `message.im`
   - `message.mpim`
5. Install the app to your workspace and copy the `xoxb-` bot token
6. Set both environment variables and run `crew gateway`

**Features**: Text messages, file sharing, thread support, message coalescing (4000 char limit).

---

### WhatsApp

WhatsApp integration uses a Node.js WebSocket bridge (e.g., [whatsapp-web.js](https://github.com/niccolozy/whatsapp-web-bridge) or similar) that connects to WhatsApp Web and exposes a WebSocket API.

**Build**:
```bash
cargo install --path crates/crew-cli --features whatsapp
```

**Config**:
```json
{
  "gateway": {
    "channels": [
      {
        "type": "whatsapp",
        "settings": {
          "bridge_url": "ws://localhost:3001"
        }
      }
    ]
  }
}
```

**Setup steps**:
1. Set up a WhatsApp Web bridge server (Node.js application that wraps whatsapp-web.js)
2. Run the bridge — it will show a QR code to scan with your WhatsApp mobile app
3. After scanning, the bridge connects to WhatsApp Web and listens on a WebSocket port (default: 3001)
4. Configure `bridge_url` to point to the bridge's WebSocket endpoint
5. Run `crew gateway`

**Features**: Text messages, media handling, message coalescing.

---

### Feishu / Lark

Feishu (Chinese) and Lark (international) are the same platform by ByteDance. crew-rs supports both via a single `feishu` channel type with a `region` setting.

**Build**:
```bash
cargo install --path crates/crew-cli --features feishu
```

**Config**:
```json
{
  "gateway": {
    "channels": [
      {
        "type": "feishu",
        "settings": {
          "app_id_env": "FEISHU_APP_ID",
          "app_secret_env": "FEISHU_APP_SECRET",
          "verification_token_env": "FEISHU_VERIFICATION_TOKEN",
          "encrypt_key_env": "FEISHU_ENCRYPT_KEY",
          "region": "feishu",
          "mode": "websocket",
          "webhook_port": 9000
        }
      }
    ]
  }
}
```

**Environment variables**:
```bash
export FEISHU_APP_ID=cli_xxxxxxxxxxxx
export FEISHU_APP_SECRET=xxxxxxxxxxxxxxxxxxxxxxxxxxxx
export FEISHU_VERIFICATION_TOKEN=xxxxxxxxxxxxxxxxxxxx
export FEISHU_ENCRYPT_KEY=xxxxxxxxxxxxxxxxxxxx
```

**Setup steps**:
1. Go to [Feishu Open Platform](https://open.feishu.cn/) (or [Lark Developer](https://open.larksuite.com/) for international)
2. Create a new app under **Create Custom App**
3. Copy the **App ID** and **App Secret** from the app's credentials page
4. Under **Event Subscriptions**:
   - Set the **Verification Token** and **Encrypt Key**
   - Subscribe to `im.message.receive_v1` event
5. Under **Permissions & Scopes**, add:
   - `im:message` — Send and receive messages
   - `im:message:send_as_bot` — Send messages as bot
   - `contact:user.base:readonly` — Read user info
6. Choose connection mode:
   - **WebSocket mode** (`"mode": "websocket"`): No public URL needed, the bot connects outbound
   - **Webhook mode** (`"mode": "webhook"`): Requires a publicly accessible URL pointing to the bot (set `webhook_port`)
7. Set the `region` to `"feishu"` (China) or `"lark"` (international) based on your platform
8. Publish/activate the app on the Feishu admin console
9. Set environment variables and run `crew gateway`

**Features**: Text messages, rich text, image/file handling, card messages, WebSocket or webhook connectivity.

---

### Email (IMAP/SMTP)

Email channel polls an IMAP inbox for new messages and sends replies via SMTP.

**Build**:
```bash
cargo install --path crates/crew-cli --features email
```

**Config**:
```json
{
  "gateway": {
    "channels": [
      {
        "type": "email",
        "allowed_senders": ["alice@example.com"],
        "settings": {
          "imap_host": "imap.gmail.com",
          "imap_port": 993,
          "smtp_host": "smtp.gmail.com",
          "smtp_port": 465,
          "username_env": "EMAIL_USERNAME",
          "password_env": "EMAIL_PASSWORD"
        }
      }
    ]
  }
}
```

**Environment variables**:
```bash
export EMAIL_USERNAME=your-email@gmail.com
export EMAIL_PASSWORD=your-app-password
```

**Setup steps** (Gmail example):
1. Enable IMAP in Gmail settings (Settings > Forwarding and POP/IMAP > Enable IMAP)
2. Generate an [App Password](https://myaccount.google.com/apppasswords) (requires 2FA enabled on the Google account)
3. Set `EMAIL_USERNAME` to your Gmail address and `EMAIL_PASSWORD` to the app password
4. Use `imap.gmail.com:993` for IMAP and `smtp.gmail.com:465` for SMTP
5. Add trusted sender emails to `allowed_senders` to restrict who can message the bot
6. Run `crew gateway`

**Features**: Plain text and HTML email parsing, attachment handling, IMAP IDLE for near-instant notification, reply threading.

---

### Twilio SMS

**Build**:
```bash
cargo install --path crates/crew-cli --features twilio
```

Twilio integration requires a Twilio account, phone number, and webhook configuration. The channel uses an HTTP webhook endpoint (via axum) to receive incoming SMS.

---

### WeCom / WeChat Work

WeCom (企业微信) is Tencent's enterprise messaging platform. The channel uses a Custom App with webhook callback for receiving messages and the WeCom REST API for sending.

**Build**:
```bash
cargo install --path crates/crew-cli --features wecom
```

**Config**:
```json
{
  "gateway": {
    "channels": [
      {
        "type": "wecom",
        "settings": {
          "corp_id_env": "WECOM_CORP_ID",
          "agent_secret_env": "WECOM_AGENT_SECRET",
          "agent_id": "1000002",
          "verification_token": "your-callback-token",
          "encoding_aes_key": "your-encoding-aes-key",
          "webhook_port": 9322
        }
      }
    ]
  }
}
```

**Environment variables**:
```bash
export WECOM_CORP_ID=your-corp-id
export WECOM_AGENT_SECRET=your-agent-secret
```

**Setup steps**:
1. Log in to the [WeCom Admin Console](https://work.weixin.qq.com/) and create a Custom App
2. Copy the **Corp ID** from the admin console and the **Agent Secret** from the app's credentials
3. Note the **Agent ID** (numeric) from the app settings
4. Under **Receive Messages**, set the callback URL to point to your server on the configured `webhook_port` (default: 9322)
5. Copy the **Token** and **EncodingAESKey** from the callback configuration page
6. Set environment variables and run `crew gateway`

**Features**: Text messages, image/file/voice media handling, message dedup, pure-Rust AES/SHA1 crypto (no external deps).

---

## Web Dashboard

The web dashboard provides a browser-based admin panel for managing multiple gateway instances, user accounts, and per-user configurations. It requires the `api` feature flag.

### Starting the Dashboard

```bash
# Build with API feature
cargo install --path crates/crew-cli --features api,telegram,discord,slack,whatsapp,feishu,email

# Start the server
crew serve --port 8080

# With auth token for remote access
crew serve --host 0.0.0.0 --port 8080 --auth-token my-secret-token

# With custom data and config directories
crew serve --data-dir ~/.crew --config /path/to/config.json
```

The dashboard is accessible at `http://localhost:8080/admin/`.

**Security notes**:
- Default bind is `127.0.0.1` (localhost only)
- When binding to `0.0.0.0` without `--auth-token`, a random token is auto-generated and printed to the console
- For production, always use `--auth-token` or configure email OTP auth

### Email OTP Authentication

The dashboard supports Larksuite-style email verification code login. Users enter their email, receive a 6-digit code, and verify it to get a session.

**Configure SMTP** in your config file (e.g., `~/Library/Application Support/crew/config.json` on macOS):

```json
{
  "dashboard_auth": {
    "smtp": {
      "host": "smtp.gmail.com",
      "port": 465,
      "username": "noreply@yourdomain.com",
      "password_env": "SMTP_PASSWORD",
      "from_address": "noreply@yourdomain.com"
    },
    "session_expiry_hours": 24,
    "allow_self_registration": true
  }
}
```

**Gmail setup**:
1. Enable 2-Factor Authentication on your Google account
2. Generate an [App Password](https://myaccount.google.com/apppasswords) (select "Mail" as the app)
3. Set the `SMTP_PASSWORD` environment variable to the generated 16-character app password
4. Use your Gmail address for both `username` and `from_address`

**Configuration options**:
- `session_expiry_hours`: How long sessions last before requiring re-login (default: 24)
- `allow_self_registration`: If `true`, any email can sign up; if `false`, only pre-created users can log in

**Dev mode**: If no `dashboard_auth` is configured, OTP codes are logged to the server console instead of being emailed. Self-registration is enabled by default in dev mode.

### User Management

Users are stored as JSON files in `{data_dir}/users/`. Each user has:
- **ID**: Slug derived from email (e.g., `alice@example.com` -> `alice`)
- **Email**: Used for OTP login
- **Name**: Display name
- **Role**: `admin` or `user`

**Admin operations** (via dashboard or API):
- Create users with specific roles
- Delete users
- Promote/demote user roles (edit the JSON file directly at `~/.crew/users/{id}.json`)

**User roles**:
- **Admin**: Full access to all profiles, user management, and system settings
- **User**: Can only manage their own profile and gateway

### Profile Management

Each user gets a profile that bundles all configuration needed to run their own gateway instance:

- **LLM Provider**: Provider name, model, API key
- **Search APIs**: Perplexity, Brave Search, You.com API keys
- **Messaging Channels**: Telegram, Discord, Slack, WhatsApp, Feishu
- **Gateway Settings**: Max history, max iterations, system prompt

**Via the dashboard**:
1. Log in with your email
2. Navigate to "My Profile"
3. Configure your LLM provider (required before starting a gateway)
4. Add messaging channel credentials
5. Click "Start" to launch your personal gateway
6. View live logs in the log viewer

The process manager spawns each gateway as a child process of the `crew serve` server, with environment variables from the profile's `env_vars` passed to the child process.

### Dashboard API Reference

**Public endpoints** (no auth required):

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/api/auth/send-code` | Send OTP code to email |
| `POST` | `/api/auth/verify` | Verify OTP code, get session token |
| `POST` | `/api/auth/logout` | Revoke session |

**User endpoints** (session token required):

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/auth/me` | Get current user + profile |
| `GET` | `/api/my/profile` | Get own profile + gateway status |
| `PUT` | `/api/my/profile` | Update own profile config |
| `POST` | `/api/my/profile/start` | Start own gateway |
| `POST` | `/api/my/profile/stop` | Stop own gateway |
| `POST` | `/api/my/profile/restart` | Restart own gateway |
| `GET` | `/api/my/profile/status` | Get own gateway process status |
| `GET` | `/api/my/profile/logs` | SSE stream of own gateway logs |

**Admin endpoints** (admin token or admin role required):

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/admin/overview` | System overview |
| `GET` | `/api/admin/profiles` | List all profiles |
| `POST` | `/api/admin/profiles` | Create profile |
| `GET` | `/api/admin/profiles/{id}` | Get profile |
| `PUT` | `/api/admin/profiles/{id}` | Update profile |
| `DELETE` | `/api/admin/profiles/{id}` | Delete profile |
| `POST` | `/api/admin/profiles/{id}/start` | Start gateway |
| `POST` | `/api/admin/profiles/{id}/stop` | Stop gateway |
| `POST` | `/api/admin/profiles/{id}/restart` | Restart gateway |
| `GET` | `/api/admin/profiles/{id}/status` | Get gateway status |
| `GET` | `/api/admin/profiles/{id}/logs` | SSE stream of gateway logs |
| `POST` | `/api/admin/start-all` | Start all enabled gateways |
| `POST` | `/api/admin/stop-all` | Stop all gateways |
| `GET` | `/api/admin/users` | List all users |
| `POST` | `/api/admin/users` | Create user |
| `DELETE` | `/api/admin/users/{id}` | Delete user |

**Authentication**: Pass session or admin token via:
- `Authorization: Bearer <token>` header
- `?token=<token>` query parameter (for SSE/EventSource)

**Chat endpoints** (user or admin auth):

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/api/chat` | Send a chat message |
| `GET` | `/api/chat/stream` | SSE stream of responses |
| `GET` | `/api/sessions` | List sessions |
| `GET` | `/api/sessions/{id}/messages` | Get session messages |
| `GET` | `/api/status` | Server status |

**Monitoring**:

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/metrics` | Prometheus metrics (public) |

---

## Multi-User Setup

By default, all data (episodes, memory, sessions, research) is stored in `~/.crew`. To run multiple users on a shared machine, override the data directory:

```bash
# Using environment variable (recommended for services)
CREW_HOME=/data/crew/alice crew gateway --config /shared/config.json
CREW_HOME=/data/crew/bob   crew gateway --config /shared/config.json

# Using CLI flag
crew chat --data-dir /data/crew/alice
crew chat --data-dir /data/crew/bob
```

Resolution order: `--data-dir` flag > `CREW_HOME` env var > `~/.crew`.

Each user gets isolated storage:

```
/data/crew/alice/
├── episodes.redb     # Episodic memory DB
├── memory/           # Long-term memory + daily notes
├── sessions/         # Conversation history (JSONL)
├── research/         # Deep search results
├── skills/           # Installed skills
├── history/          # Readline history
├── profiles/         # User profiles (dashboard)
├── users/            # User accounts (dashboard)
└── cron.json         # Scheduled jobs
```

When using the dashboard (`crew serve`), each user profile gets its own data directory at `~/.crew/profiles/{user-id}/data/`, with isolated memory, sessions, and research storage.

---

## Tools

| Tool | Description |
|------|-------------|
| `shell` | Execute shell commands with SafePolicy (blocks dangerous commands like `rm -rf /`) |
| `read_file` | Read file contents (symlink-safe with `O_NOFOLLOW` on Unix) |
| `write_file` | Write/create files |
| `edit_file` | Edit files with search/replace |
| `glob` | Find files by pattern |
| `grep` | Search file contents with regex |
| `list_dir` | List directory contents |
| `web_search` | Internet search (requires Perplexity, Brave, or You.com API key) |
| `web_fetch` | Fetch and parse web content (SSRF-protected) |
| `message` | Send cross-channel messages |
| `spawn` | Launch background subagents |
| `cron` | Schedule recurring tasks |
| `browser` | Headless Chrome automation |
| `deep_search` | Multi-round web research with result persistence |
| `save_memory` | Save information to long-term memory |
| `recall_memory` | Recall information from memory |
| `diff_edit` | Diff-based file editing with search/replace |
| `send_file` | Send file attachments to chat channels |
| `switch_model` | Switch LLM model at runtime |
| `run_pipeline` | Execute DOT-based pipeline workflows |
| `configure_tool` | Runtime tool configuration |

**Tool policies**: Control which tools are available via allow/deny lists:

```json
{
  "tool_policy": {
    "allow": ["shell", "read_file", "write_file"],
    "deny": ["browser"]
  }
}
```

Named groups: `group:fs` (read_file/write_file/edit_file/diff_edit), `group:runtime` (shell), `group:search` (glob/grep/list_dir), `group:web` (web_search/web_fetch/browser), `group:sessions` (spawn).

---

## Account Management

Sub-accounts inherit LLM provider config from a parent profile but have their own data directory (memory, sessions, episodes, skills) and channels. See `crew account` CLI commands above.

---

## Office Tools

Native Rust office file manipulation for DOCX, PPTX, and XLSX files. Replaces Python scripts with `zip` + `quick-xml`. See `crew office` CLI commands above.

---

## Memory System

crew-rs has a hybrid memory system:

- **Episodic memory**: Stored in `episodes.redb` (redb database). Task completion summaries for learning from past experiences.
- **Long-term memory**: `MEMORY.md` file for persistent notes and preferences.
- **Daily notes**: Date-keyed notes for context.
- **Hybrid search**: BM25 + vector cosine similarity (HNSW index via `hnsw_rs`). Configurable weights (default: 0.7 vector / 0.3 BM25). Falls back to BM25-only without embedding provider.

Configure embeddings for vector search:

```json
{
  "embedding": {
    "provider": "openai",
    "api_key_env": "OPENAI_API_KEY"
  }
}
```

---

## Skills System

Skills are markdown files with YAML frontmatter that define reusable prompts and workflows:

```markdown
---
name: code-review
description: Review code for best practices
trigger: /review
---

Review the provided code for:
1. Security vulnerabilities
2. Performance issues
3. Code style
```

**Built-in skills**: 6 pre-installed skills. **Custom skills** are stored in `.crew/skills/` or `~/.crew/skills/`.

```bash
crew skills list                          # List all skills
crew skills install user/repo/skill       # Install from GitHub
crew skills remove skill-name             # Remove a skill
```

---

## Sandbox Isolation

Three backends for isolating shell commands:

| Backend | Platform | Description |
|---------|----------|-------------|
| `Bwrap` | Linux | bubblewrap namespace isolation |
| `Macos` | macOS | sandbox-exec (Seatbelt) |
| `Docker` | All | Container isolation with resource limits |

```json
{
  "sandbox": {
    "enabled": true,
    "allow_network": true
  }
}
```

Docker sandbox supports mount modes (none/ro/rw), resource limits (CPU/memory/PIDs), and network isolation. Auto-detection (`SandboxMode::Auto`) selects the best available backend.

---

## Hooks

Lifecycle hooks run shell commands at agent events:

| Event | Description |
|-------|-------------|
| `before_tool_call` | Before a tool is executed (can deny with exit code 1) |
| `after_tool_call` | After a tool completes |
| `before_llm_call` | Before calling the LLM |
| `after_llm_call` | After the LLM responds |

```json
{
  "hooks": [
    {
      "event": "before_tool_call",
      "command": ["~/scripts/approve.sh"],
      "timeout_ms": 5000,
      "tool_filter": "shell"
    }
  ]
}
```

Shell protocol: JSON payload on stdin, exit code semantics (0=allow, 1=deny, 2+=error). Circuit breaker auto-disables hooks after 3 consecutive failures.

---

## Cron & Heartbeat

Schedule recurring tasks:

```json
{
  "gateway": {
    "channels": [{"type": "cli"}]
  }
}
```

Then use the `cron` tool in conversation or the `crew cron` CLI:

```bash
# Cron expression (7-field: sec min hour dom month dow year)
crew cron add --name "morning-report" --message "Generate daily report" --cron "0 0 9 * * * *"

# Interval-based (every N seconds)
crew cron add --name "health-check" --message "Check API health" --every 3600

# One-shot
crew cron add --name "reminder" --message "Submit the PR" --once-at "2025-01-15T10:00:00Z"
```

The heartbeat service runs periodic background checks independently of cron jobs.

---

## Architecture

```
crew-rs/
  crates/
    crew-core/         # Types, task model, message protocols, UTF-8 utilities
    crew-memory/       # Episodic memory (redb), memory store, hybrid BM25+vector search
    crew-llm/          # LLM provider abstraction (14 provider registry entries)
    crew-agent/        # Agent runtime, tool system, sandbox, MCP, compaction, hooks, plugins
    crew-bus/          # Message bus, channels, sessions, cron, heartbeat, coalescing
    crew-cli/          # CLI interface, API server, dashboard, profiles, user management, OTP auth
    crew-pipeline/     # DOT-based pipeline parser, executor, and tool integration
    app-skills/        # 7 bundled app-skills (news, deep-search, deep-crawl, send-email, weather, account-manager, time)
    platform-skills/   # Platform-specific skills (asr)
  dashboard/           # React 19 + Vite + Tailwind CSS web dashboard
```

**Agent loop** (`crew-agent/src/agent.rs`):
1. Build messages (system prompt + conversation history + memory context)
2. Call LLM with tool specs (filtered by ToolPolicy + provider policy)
3. If tool calls returned -> execute tools -> append results -> loop
4. If EndTurn or budget exceeded -> return result
5. Context compaction kicks in when token budget fills

**Key design decisions**:
- Pure Rust TLS via `rustls` (no OpenSSL dependency)
- `eyre`/`color-eyre` for error handling
- `Arc<dyn Trait>` for shared providers/tools/reporters
- `AtomicBool` for shutdown signaling
- Symlink-safe file I/O via `O_NOFOLLOW` on Unix
- Constant-time auth token comparison
- Atomic file writes (write-then-rename) for crash safety

---

## Development

```bash
cargo build --workspace           # Build all crates
cargo test --workspace            # Run all tests
cargo test -p crew-agent          # Test single crate
cargo test -p crew-cli test_name  # Run single test
cargo clippy --workspace          # Lint
cargo fmt --all                   # Format
cargo fmt --all -- --check        # Check formatting
./scripts/pre-release.sh          # Full pre-release smoke test
```

---

## License

Apache-2.0
