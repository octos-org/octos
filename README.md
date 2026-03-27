# Octos

> Like an octopus — 9 brains (1 central + 8 in each arm), every arm thinks independently, but they share one brain.

**Open Cognitive Tasks Orchestration System** — a Rust-native, API-first Agentic OS.

31MB static binary. 91 REST endpoints. 14 LLM providers. 14 messaging channels. Multi-tenant. Zero dependencies.

## Why Octos

Most agentic systems are single-tenant chat assistants — one user, one model, one conversation at a time. Octos is different:

- **API-first Agentic OS**: 91 REST endpoints (chat, sessions, admin, profiles, skills, metrics, webhooks). Any frontend — web, mobile, CLI, CI/CD — can be built on top. Not locked to a chat window.
- **Multi-tenant by design**: One 31MB binary serves 200+ profiles on a 16GB machine. Each profile runs as a separate OS process with isolated memory, sessions, and data. Family Plan sub-accounts let parents share config with children.
- **Multi-LLM DOT pipelines**: Define workflows as DOT graphs. Each pipeline node can use a different LLM — cheap models for search, strong models for synthesis. Dynamic parallel fan-out spawns N concurrent workers at runtime.
- **3-layer provider failover**: RetryProvider → ProviderChain → AdaptiveRouter. Hedge mode races 2 providers simultaneously. Lane mode scores providers by latency/error/quality/cost. Circuit breakers auto-disable degraded providers.
- **LRU tool deferral**: 15 active tools for fast LLM reasoning, 34+ available on demand. The LLM calls `activate_tools` to load specialized tools mid-conversation. Idle tools auto-evict. No other agentic framework does this.
- **5 queue modes per session**: Followup (FIFO), Collect (batch), Steer (latest only), Interrupt (cancel current), Speculative (concurrent agent when slow). Users control concurrency behavior in real-time via `/queue`.
- **Session control in any channel**: `/new`, `/s <name>`, `/sessions`, `/back` — works in Telegram, Discord, Slack, WhatsApp. Background tasks buffer up to 50 messages; switch back and they flush automatically.
- **3-layer memory**: Long-term (MEMORY.md + entity bank with abstracts, auto-injected), episodic (task outcomes in redb, vector-searchable by working directory), session (JSONL + LLM compaction at 40+ messages).
- **Native office suite**: PPTX/DOCX/XLSX manipulation via pure Rust (zip + quick-xml). No LibreOffice dependency for basic operations.
- **Sandbox isolation**: bwrap (Linux) + sandbox-exec (macOS) + Docker. `deny(unsafe_code)` workspace-wide. 67 prompt injection tests. macOS Keychain for key storage. Constant-time token comparison.

## Documentation / 文档

**[User Guide (English)](docs/user-guide.md)** | **[用户指南 (中文)](docs/user-guide-zh.md)** | **[中文 README](README-zh.md)**

Comprehensive guides covering dashboard setup, LLM providers, tool configuration, profile management, bundled skills, platform skills (ASR/TTS), custom skill development, and more.

---

## Table of Contents

- [Why Octos](#why-octos)
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

### Core Architecture
- **31MB static binary**: Pure Rust, zero runtime dependencies, builds on Linux x86_64, macOS ARM64, and Docker Alpine
- **91 REST endpoints**: Full API for chat, sessions, admin, profiles, skills, metrics, webhooks, platform skills — build any UI on top
- **SSE broadcasting**: Real-time tool events, LLM token streaming, and progress updates to any subscriber
- **Prometheus + JSON metrics**: CPU, memory, per-provider latency, P95 — production-grade observability

### LLM & Routing
- **14 LLM providers**: Anthropic, OpenAI, Gemini, OpenRouter, DeepSeek, Groq, Moonshot/Kimi, DashScope/Qwen, MiniMax, Zhipu/GLM, Z.AI, Nvidia NIM, Ollama, vLLM
- **3-layer failover**: RetryProvider (exponential backoff) → ProviderChain (multi-provider with circuit breaker) → AdaptiveRouter (metrics-driven scoring)
- **Adaptive routing modes**: Off (static), Hedge (race 2 providers, take winner), Lane (score-based: latency 35%, error 30%, quality 20%, cost 15%)
- **QoS ranking**: Orthogonal quality toggle — combine with any routing mode
- **Auto-escalation**: Detects slow responses and auto-enables hedge + speculative queue
- **Provider auto-detect**: `--model gpt-4o` automatically selects OpenAI
- **Model catalog**: Programmatic discovery with capabilities, costs, and alias-based lookup

### Multi-Tenant & Sub-Accounts
- **High-density multi-tenancy**: 200+ profiles on 16GB Mac Mini. Each profile is a separate OS process with isolated memory, sessions, and data
- **Family Plan sub-accounts**: Parent profiles share config with child profiles. `octos account create/start/stop`
- **Web dashboard**: React SPA with per-user profile management, gateway controls, and live log streaming
- **Email OTP auth**: Larksuite-style verification code login
- **OAuth login**: PKCE browser flow, device code flow, or paste-token
- **Fleet management**: REST API for starting/stopping/monitoring all profiles programmatically

### Channels (14 built-in)
- **Multi-channel gateway**: CLI, Telegram, Discord, Slack, WhatsApp, Feishu/Lark, Email (IMAP/SMTP), Twilio SMS, WeCom, WeCom Bot, WeChat, Matrix, QQ Bot, API
- **Session control**: `/new`, `/s <name>`, `/sessions`, `/back`, `/delete` — works in every channel
- **Pending messages**: Up to 50 messages buffered when session inactive, auto-flushed on switch
- **Completion notifications**: Background tasks notify when done across sessions
- **Cross-channel messaging**: Send messages from any channel to any other
- **Message coalescing**: Channel-aware response splitting (Telegram 4096, Discord 2000, Slack limits)
- **Media handling**: Auto-download photos, voice, audio, documents from all channels
- **Vision support**: Send images to vision-capable LLMs (Anthropic, OpenAI, Gemini, OpenRouter)

### Agent Concurrency (5 Queue Modes)
- **Followup**: FIFO — one message at a time
- **Collect** (default): Batch all queued messages into single prompt
- **Steer**: Keep only the newest message, discard older
- **Interrupt**: Cancel current agent loop, process new message immediately
- **Speculative**: Spawn concurrent agent task when slow — user never blocked, both results delivered
- Per-session control via `/queue` command

### Tools & LRU Deferral
- **13 built-in tools**: Shell, read/write/edit files, glob, grep, list_dir, web search/fetch, git, browser, code structure
- **12 agent-level tools**: Activate tools, spawn, deep search, synthesize, save/recall memory, manage skills, configure tools, messaging, cron
- **8 bundled app-skills**: news, deep-search, deep-crawl, send-email, weather, account-manager, clock, voice
- **LRU tool deferral**: 15 active max, idle auto-evict after 5 iterations. `activate_tools` loads specialized tools on demand. Unlimited catalog, finite context window.
- **SafePolicy**: Deny rm -rf, dd, mkfs, fork bomb. Ask on sudo, git push --force
- **Concurrent execution**: All tool calls run in parallel via `join_all()`
- **Plugin system**: stdin/stdout JSON protocol — write plugins in any language

### Pipeline Orchestration
- **DOT-based workflows**: Define pipelines as Graphviz digraph with node attributes
- **Per-node model selection**: Each node can use a different LLM (cheap for search, strong for synthesis)
- **Dynamic parallel fan-out**: `DynamicParallel` handler — LLM planner generates N sub-tasks, executed concurrently
- **Handler types**: LLM, CodeGen, DynamicParallel, Parallel, Converge
- **Quality gates**: `goal_gate` checks output quality before proceeding
- **Context fidelity**: Configurable context passing between nodes

### Memory (3-Layer)
- **Long-term memory**: MEMORY.md + daily notes (YYYY-MM-DD.md) + entity bank (bank/entities/*.md with abstracts). Auto-injected into system prompt
- **Episodic memory**: Task outcomes (success/failure/blocked) stored in redb with files modified, key decisions. Vector-searchable by working directory
- **Session memory**: JSONL transcripts with LRU cache (1000 sessions). LLM compaction at 40+ messages (keeps last 10 intact, atomic rewrite)
- **Hybrid search**: HNSW vector index (16 connections, 10K capacity) + BM25 inverted index. Cosine 0.7 + BM25 0.3 blend
- **Memory tools**: `save_memory` + `recall_memory` (entity-based, with merge-before-save rule)

### Search
- **6-provider failover**: Tavily → DuckDuckGo → Exa → Brave → You.com → Perplexity
- **Deep search**: Parallel multi-query with 8 concurrent workers
- **Deep research**: DOT pipeline — plan → search → analyze → synthesize (multi-LLM)
- **Site crawl**: Full-site crawling with depth and page limits

### Security
- **Sandbox isolation**: bwrap (Linux) + sandbox-exec (macOS) + Docker with resource limits
- **`deny(unsafe_code)`**: Workspace-wide, compile-time enforced
- **67 prompt injection tests**: prompt_guard + sanitize modules
- **macOS Keychain**: Secure key storage via `security` CLI
- **Constant-time comparison**: `constant_time_eq` + `subtle` crate
- **Tool policies**: Allow/deny lists, wildcard matching, named groups, provider-specific filtering
- **SSRF protection**: Block private IP ranges
- **Env sanitization**: Block sensitive environment variables

### Voice & Office
- **TTS**: Qwen3-TTS via voice-skill (voice cloning via reference audio)
- **ASR**: Qwen3-ASR via ominix-api platform skill
- **Office tools**: Native PPTX/DOCX/XLSX manipulation (zip + quick-xml) — extract, pack, validate, add slides, accept tracked changes
- **Installable skills**: mofa-cards, mofa-comic, mofa-infographic, mofa-slides (17 styles, 4K) via octos-hub

### Developer Experience
- **1,477 tests**: Unit (15s) + integration (5min)
- **cargo fmt + clippy**: Enforced in CI with `-D warnings`
- **Typed LLM errors**: Structured hierarchy (rate limit, auth, context overflow) with retryability
- **LLM middleware**: Composable interceptors (logging, cost tracking, caching)
- **High-level client**: `generate()`, `generate_object()`, `generate_typed<T>()`, `stream()` APIs
- **Config migration**: Versioned config with automatic migration
- **Config hot-reload**: SHA-256 change detection, live system prompt updates
- **MCP integration**: JSON-RPC stdio transport for Model Context Protocol servers
- **Pure Rust TLS**: No OpenSSL dependency (rustls)

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
git clone https://github.com/octos-org/octos.git
cd octos

# Quick local deploy (builds, installs, sets up service)
./scripts/local-deploy.sh --full    # All features + dashboard + skills
./scripts/local-deploy.sh --minimal # CLI + chat only

# Or install manually:
cargo install --path crates/octos-cli

# Install with all messaging channels + dashboard
cargo install --path crates/octos-cli --features api,telegram,discord,slack,whatsapp,feishu,email

# Install with specific channels
cargo install --path crates/octos-cli --features telegram,discord

# Install with browser automation (requires Chrome/Chromium)
cargo install --path crates/octos-cli --features browser

# Or build locally without installing
cargo build --release
./target/release/octos --help
```

See **[Local Deployment Guide](docs/LOCAL_DEPLOYMENT.md)** for platform-specific instructions (macOS, Linux, Windows/WSL2), background service setup, and troubleshooting.

### Feature Flags

| Feature | Description |
|---------|-------------|
| `api` | Web dashboard + REST API server (`octos serve`) |
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
octos init

# Set your API key (or use OAuth login)
export ANTHROPIC_API_KEY=your-key-here
# Or: octos auth login -p anthropic

# Interactive chat
octos chat

# Single-message mode (non-interactive)
octos chat --message "Add a hello function to lib.rs"

# Check system status
octos status
```

---

## CLI Commands

### `octos chat`

Interactive multi-turn conversation:

```bash
octos chat                          # Default provider (Anthropic)
octos chat --provider openai        # Use OpenAI
octos chat --model gpt-4o           # Auto-detects OpenAI from model name
octos chat --verbose                # Show tool outputs
octos chat --message "Fix the bug"  # Single message, non-interactive
```

### `octos gateway`

Run as a persistent multi-channel messaging daemon:

```bash
octos gateway                       # Uses config from .octos/config.json
octos gateway --provider openai     # Override provider
octos gateway --verbose             # Verbose logging
octos gateway --data-dir /data/bob  # Custom data directory
```

### `octos serve`

Start the web dashboard + REST API server (requires `api` feature):

```bash
octos serve                         # Listen on 127.0.0.1:8080
octos serve --port 8090             # Custom port
octos serve --host 0.0.0.0          # Accept connections from all interfaces
octos serve --auth-token my-secret  # Set admin auth token
octos serve --data-dir ~/.octos      # Custom data directory
octos serve --config /path/to/config.json
```

### `octos init`

Initialize workspace with config and bootstrap files:

```bash
octos init              # Interactive setup
octos init --defaults   # Use defaults (Anthropic/Claude)
```

Creates:
- `.octos/config.json` - Configuration
- `.octos/AGENTS.md` - Agent instructions
- `.octos/SOUL.md` - Personality definition
- `.octos/USER.md` - User preferences
- `.octos/memory/`, `.octos/sessions/`, `.octos/skills/` directories

### `octos status`

Show system status (config, API keys, bootstrap files).

### `octos auth`

OAuth login and API key management:

```bash
octos auth login --provider openai         # PKCE browser OAuth flow
octos auth login --provider openai --device-code  # Device code flow
octos auth login --provider anthropic      # Paste-token flow
octos auth logout --provider openai        # Remove stored credential
octos auth status                          # Show authenticated providers
```

### `octos cron`

Manage scheduled cron jobs:

```bash
octos cron list                          # List active jobs
octos cron list --all                    # Include disabled jobs
octos cron add --name "daily" --message "Run report" --cron "0 0 9 * * * *"
octos cron add --name "check" --message "Check status" --every 3600
octos cron remove <job-id>
octos cron enable <job-id>               # Enable a job
octos cron enable <job-id> --disable     # Disable a job
```

### `octos skills`

Manage skills:

```bash
octos skills list                          # List installed skills
octos skills install user/repo/skill-name  # Install from GitHub
octos skills remove skill-name             # Remove a skill
```

### `octos channels status`

Show configured gateway channels and their compile/config status.

### `octos account`

Manage sub-accounts under profiles:

```bash
octos account list --profile <id>     # List sub-accounts
octos account create --profile <id> <name>  # Create sub-account
octos account update <id>             # Update sub-account
octos account delete <id>             # Delete sub-account
octos account info <id>               # Show sub-account details
octos account start <id>              # Start sub-account gateway
octos account stop <id>               # Stop sub-account gateway
```

### `octos office`

Office file manipulation:

```bash
octos office extract <file>           # Extract text from DOCX/PPTX/XLSX
octos office unpack <file> <dir>      # Unpack archive to directory
octos office pack <dir> <output>      # Pack directory into archive
octos office clean <dir>              # Remove orphaned files from unpacked PPTX
octos office add-slide <dir> <source> # Add slide by duplicating or from layout
octos office validate <path>          # Validate document structure
```

### Other Commands

```bash
octos clean [--all]       # Clean up state/database files
octos completions <shell> # Generate shell completions (bash/zsh/fish)
```

---

## Configuration

### Config File Locations

Config is loaded in this priority order:

1. **CLI flag**: `--config /path/to/config.json`
2. **Project-local**: `.octos/config.json` (in current directory)
3. **Global**: Platform-specific config directory:
   - **macOS**: `~/Library/Application Support/octos/config.json`
   - **Linux**: `~/.config/octos/config.json`
   - **Windows**: `%APPDATA%\octos\config.json`

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
cargo install --path crates/octos-cli --features telegram
```

**Config** (`.octos/config.json`):
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
5. Run `octos gateway`

**Features**: Text messages, photo/document/voice/audio download, vision (sends images to LLM), voice transcription (via Qwen3-ASR), message coalescing (4096 char limit).

---

### Discord

**Prerequisites**: Create a Discord application at the [Discord Developer Portal](https://discord.com/developers/applications) and get a bot token.

**Build**:
```bash
cargo install --path crates/octos-cli --features discord
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
6. Set `DISCORD_BOT_TOKEN` and run `octos gateway`

**Features**: Text messages, attachment handling, message coalescing (2000 char limit).

---

### Slack

**Prerequisites**: Create a Slack app with Socket Mode enabled.

**Build**:
```bash
cargo install --path crates/octos-cli --features slack
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
6. Set both environment variables and run `octos gateway`

**Features**: Text messages, file sharing, thread support, message coalescing (4000 char limit).

---

### WhatsApp

WhatsApp integration uses a Node.js WebSocket bridge (e.g., [whatsapp-web.js](https://github.com/niccolozy/whatsapp-web-bridge) or similar) that connects to WhatsApp Web and exposes a WebSocket API.

**Build**:
```bash
cargo install --path crates/octos-cli --features whatsapp
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
5. Run `octos gateway`

**Features**: Text messages, media handling, message coalescing.

---

### Feishu / Lark

Feishu (Chinese) and Lark (international) are the same platform by ByteDance. octos supports both via a single `feishu` channel type with a `region` setting.

**Build**:
```bash
cargo install --path crates/octos-cli --features feishu
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
9. Set environment variables and run `octos gateway`

**Features**: Text messages, rich text, image/file handling, card messages, WebSocket or webhook connectivity.

---

### Email (IMAP/SMTP)

Email channel polls an IMAP inbox for new messages and sends replies via SMTP.

**Build**:
```bash
cargo install --path crates/octos-cli --features email
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
6. Run `octos gateway`

**Features**: Plain text and HTML email parsing, attachment handling, IMAP IDLE for near-instant notification, reply threading.

---

### Twilio SMS

**Build**:
```bash
cargo install --path crates/octos-cli --features twilio
```

Twilio integration requires a Twilio account, phone number, and webhook configuration. The channel uses an HTTP webhook endpoint (via axum) to receive incoming SMS.

---

### WeCom / WeChat Work

WeCom (企业微信) is Tencent's enterprise messaging platform. The channel uses a Custom App with webhook callback for receiving messages and the WeCom REST API for sending.

**Build**:
```bash
cargo install --path crates/octos-cli --features wecom
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
6. Set environment variables and run `octos gateway`

**Features**: Text messages, image/file/voice media handling, message dedup, pure-Rust AES/SHA1 crypto (no external deps).

---

## Web Dashboard

The web dashboard provides a browser-based admin panel for managing multiple gateway instances, user accounts, and per-user configurations. It requires the `api` feature flag.

### Starting the Dashboard

```bash
# Build with API feature
cargo install --path crates/octos-cli --features api,telegram,discord,slack,whatsapp,feishu,email

# Start the server
octos serve --port 8080

# With auth token for remote access
octos serve --host 0.0.0.0 --port 8080 --auth-token my-secret-token

# With custom data and config directories
octos serve --data-dir ~/.octos --config /path/to/config.json
```

The dashboard is accessible at `http://localhost:8080/admin/`.

**Security notes**:
- Default bind is `127.0.0.1` (localhost only)
- When binding to `0.0.0.0` without `--auth-token`, a random token is auto-generated and printed to the console
- For production, always use `--auth-token` or configure email OTP auth

### Email OTP Authentication

The dashboard supports Larksuite-style email verification code login. Users enter their email, receive a 6-digit code, and verify it to get a session.

**Configure SMTP** in your config file (e.g., `~/Library/Application Support/octos/config.json` on macOS):

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
- Promote/demote user roles (edit the JSON file directly at `~/.octos/users/{id}.json`)

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

The process manager spawns each gateway as a child process of the `octos serve` server, with environment variables from the profile's `env_vars` passed to the child process.

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

By default, all data (episodes, memory, sessions, research) is stored in `~/.octos`. To run multiple users on a shared machine, override the data directory:

```bash
# Using environment variable (recommended for services)
OCTOS_HOME=/data/octos/alice octos gateway --config /shared/config.json
OCTOS_HOME=/data/octos/bob   octos gateway --config /shared/config.json

# Using CLI flag
octos chat --data-dir /data/octos/alice
octos chat --data-dir /data/octos/bob
```

Resolution order: `--data-dir` flag > `OCTOS_HOME` env var > `~/.octos`.

Each user gets isolated storage:

```
/data/octos/alice/
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

When using the dashboard (`octos serve`), each user profile gets its own data directory at `~/.octos/profiles/{user-id}/data/`, with isolated memory, sessions, and research storage.

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

Sub-accounts inherit LLM provider config from a parent profile but have their own data directory (memory, sessions, episodes, skills) and channels. See `octos account` CLI commands above.

---

## Office Tools

Native Rust office file manipulation for DOCX, PPTX, and XLSX files. Replaces Python scripts with `zip` + `quick-xml`. See `octos office` CLI commands above.

---

## Memory System

octos has a hybrid memory system:

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

**Built-in skills**: 6 pre-installed skills. **Custom skills** are stored in `.octos/skills/` or `~/.octos/skills/`.

```bash
octos skills list                          # List all skills
octos skills install user/repo/skill       # Install from GitHub
octos skills remove skill-name             # Remove a skill
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

Then use the `cron` tool in conversation or the `octos cron` CLI:

```bash
# Cron expression (7-field: sec min hour dom month dow year)
octos cron add --name "morning-report" --message "Generate daily report" --cron "0 0 9 * * * *"

# Interval-based (every N seconds)
octos cron add --name "health-check" --message "Check API health" --every 3600

# One-shot
octos cron add --name "reminder" --message "Submit the PR" --once-at "2025-01-15T10:00:00Z"
```

The heartbeat service runs periodic background checks independently of cron jobs.

---

## Architecture

```
octos/
  crates/
    octos-core/         # Types, task model, message protocols, UTF-8 utilities
    octos-memory/       # Episodic memory (redb), memory store, hybrid BM25+vector search
    octos-llm/          # LLM provider abstraction (14 provider registry entries)
    octos-agent/        # Agent runtime, tool system, sandbox, MCP, compaction, hooks, plugins
    octos-bus/          # Message bus, channels, sessions, cron, heartbeat, coalescing
    octos-cli/          # CLI interface, API server, dashboard, profiles, user management, OTP auth
    octos-pipeline/     # DOT-based pipeline parser, executor, and tool integration
    app-skills/        # 7 bundled app-skills (news, deep-search, deep-crawl, send-email, weather, account-manager, time)
    platform-skills/   # Platform-specific skills (asr)
  dashboard/           # React 19 + Vite + Tailwind CSS web dashboard
```

**Agent loop** (`octos-agent/src/agent.rs`):
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
cargo test -p octos-agent          # Test single crate
cargo test -p octos-cli test_name  # Run single test
cargo clippy --workspace          # Lint
cargo fmt --all                   # Format
cargo fmt --all -- --check        # Check formatting
./scripts/pre-release.sh          # Full pre-release smoke test
```

---

## License

Apache-2.0
