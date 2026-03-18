# User Manual: octos

## Table of Contents

1. [Introduction](#introduction)
2. [Installation](#installation)
3. [Quick Start](#quick-start)
4. [Configuration](#configuration)
5. [Commands Reference](#commands-reference)
6. [Working with Providers](#working-with-providers)
7. [Gateway Mode](#gateway-mode)
8. [Memory & Skills](#memory--skills)
9. [Advanced Usage](#advanced-usage)
10. [Troubleshooting](#troubleshooting)

---

## Introduction

octos is a Rust-native AI agent framework that operates in two modes:

- **Chat mode** (`octos chat`): Interactive multi-turn conversation with tools (or single-message via `--message`)
- **Gateway mode** (`octos gateway`): Persistent daemon serving multiple messaging channels

### Key Concepts

| Term | Description |
|------|-------------|
| **Agent** | AI that executes tasks using tools |
| **Tool** | A capability (shell, file ops, search, messaging) |
| **Provider** | LLM API service (Anthropic, OpenAI, etc.) |
| **Channel** | Messaging platform (CLI, Telegram, Slack, etc.) |
| **Session** | Conversation history per channel:chat_id |
| **Sandbox** | Isolated execution environment (bwrap, macOS sandbox-exec, Docker) |
| **Tool Policy** | Allow/deny rules controlling which tools are available |
| **Skill** | Reusable instruction template (SKILL.md) |
| **Bootstrap** | Context files loaded into system prompt (AGENTS.md, SOUL.md, etc.) |

---

## Installation

### Prerequisites

- Rust 1.85.0 or later
- An API key from at least one supported provider
- (Optional) Node.js, `npm install -g pptxgenjs` — PPTX creation skill
- (Optional) `ffmpeg` — video/animation skills (mofa-pptx)
- (Optional) LibreOffice, Poppler (`pdftoppm`) — office document conversion and visual QA
- (Optional) Chrome/Chromium — browser automation

### From Source

```bash
git clone https://github.com/octos-org/octos
cd octos

# Basic (CLI, chat, run, gateway with CLI channel)
cargo install --path crates/octos-cli

# With messaging channels
cargo install --path crates/octos-cli --features telegram,discord,slack,whatsapp,feishu,email,wecom

# With browser automation (requires Chrome/Chromium)
cargo install --path crates/octos-cli --features browser

# Verify
octos --version
```

### Docker

```bash
docker compose --profile gateway up -d
```

### API Keys

```bash
# Anthropic (Claude) - recommended
export ANTHROPIC_API_KEY="sk-ant-..."

# OpenAI
export OPENAI_API_KEY="sk-..."

# Or any other supported provider (see Providers section)
# Or use OAuth: octos auth login --provider openai
```

Add to `~/.bashrc` or `~/.zshrc` for persistence.

---

## Quick Start

```bash
# 1. Initialize workspace
cd your-project
octos init

# 2. Check setup
octos status

# 3. Start chatting
octos chat

# 4. Or send a single message
octos chat --message "Add a hello function to lib.rs"
```

---

## Configuration

### Config Files

Loaded in order (first found wins):
1. `.octos/config.json` (project-local)
2. `~/.config/octos/config.json` (global)

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
      {"type": "cli"},
      {"type": "telegram", "allowed_senders": ["123456789"]},
      {"type": "discord", "settings": {"token_env": "DISCORD_BOT_TOKEN"}},
      {"type": "slack", "settings": {"bot_token_env": "SLACK_BOT_TOKEN", "app_token_env": "SLACK_APP_TOKEN"}},
      {"type": "whatsapp", "settings": {"bridge_url": "ws://localhost:3001"}},
      {"type": "feishu", "settings": {"app_id_env": "FEISHU_APP_ID", "app_secret_env": "FEISHU_APP_SECRET"}}
    ],
    "max_history": 50,
    "system_prompt": "You are a helpful assistant."
  }
}
```

### Environment Variable Expansion

Use `${VAR_NAME}` syntax:

```json
{
  "base_url": "${ANTHROPIC_BASE_URL}",
  "model": "${OCTOS_MODEL}"
}
```

---

## Commands Reference

### `octos chat`

Interactive multi-turn conversation with readline history.

```bash
octos chat [OPTIONS]

Options:
  -c, --cwd <PATH>         Working directory
      --config <PATH>      Config file path
      --provider <NAME>    LLM provider
      --model <NAME>       Model name
      --base-url <URL>     Custom API endpoint
  -m, --message <MSG>      Single message (non-interactive)
      --max-iterations <N> Max tool iterations per message (default: 50)
  -v, --verbose            Show tool outputs
      --no-retry           Disable retry
```

Features:
- Arrow keys, line editing (rustyline)
- Persistent history at `.octos/history/chat_history`
- Exit: `/exit`, `/quit`, `exit`, `quit`, `:q`, Ctrl+C, Ctrl+D
- Full tool access (shell, files, search, web)

```bash
octos chat                              # Interactive (default)
octos chat --provider deepseek          # Use DeepSeek
octos chat --model glm-4-plus           # Auto-detects Zhipu
octos chat --message "Fix auth bug"     # Single message, exit
```

---

### `octos gateway`

Run as a persistent multi-channel daemon.

```bash
octos gateway [OPTIONS]

Options:
  -c, --cwd <PATH>         Working directory
      --config <PATH>      Config file path
      --provider <NAME>    Override provider
      --model <NAME>       Override model
      --base-url <URL>     Override API endpoint
  -v, --verbose            Verbose logging
      --no-retry           Disable retry
```

Requires `gateway` section in config with `channels` array. Runs continuously until Ctrl+C.

---

### `octos init`

Initialize workspace with config and bootstrap files.

```bash
octos init [OPTIONS]

Options:
  -c, --cwd <PATH>    Working directory
      --defaults       Skip prompts, use defaults
```

Creates:
- `.octos/config.json` - Provider/model config
- `.octos/.gitignore` - Ignores state files
- `.octos/AGENTS.md` - Agent instructions template
- `.octos/SOUL.md` - Personality template
- `.octos/USER.md` - User info template
- `.octos/memory/` - Memory storage directory
- `.octos/sessions/` - Session history directory
- `.octos/skills/` - Custom skills directory

---

### `octos status`

Show system status.

```bash
octos status [OPTIONS]

Options:
  -c, --cwd <PATH>    Working directory
```

```
octos Status
══════════════════════════════════════════════════

Config:    .octos/config.json (found)
Workspace: .octos/            (found)
Provider:  anthropic
Model:     claude-sonnet-4-20250514

API Keys
──────────────────────────────────────────────────
  Anthropic    ANTHROPIC_API_KEY         set
  OpenAI       OPENAI_API_KEY           not set
  ...

Bootstrap Files
──────────────────────────────────────────────────
  AGENTS.md        found
  SOUL.md          found
  USER.md          found
  TOOLS.md         missing
  IDENTITY.md      missing
```

---

### Other Commands

```bash
octos clean [--all] [--dry-run] # Clean database files
octos completions <shell>       # Generate completions (bash/zsh/fish/powershell)
octos docs                      # Generate tool + provider documentation
octos cron list [--all]         # List cron jobs
octos cron add [OPTIONS]        # Add a cron job
octos cron remove <job-id>      # Remove a cron job
octos cron enable <job-id>      # Enable/disable a cron job
octos channels status           # Show channel compile/config status
octos channels login            # WhatsApp QR code login
```

### `octos office`

Office file manipulation (DOCX/PPTX/XLSX). Native Rust replacements for Python scripts.

```bash
octos office extract <file>               # Extract text as Markdown
octos office unpack <file> <output-dir>   # Unpack into pretty-printed XML
octos office pack <input-dir> <output>    # Pack directory into Office file
octos office clean <dir>                  # Remove orphaned files from unpacked PPTX
```

### `octos account`

Manage sub-accounts under profiles. Sub-accounts inherit LLM provider config but have their own data directory (memory, sessions, skills) and channels.

```bash
octos account list --profile <id>                         # List sub-accounts
octos account create --profile <id> <name> [OPTIONS]      # Create sub-account
octos account update <id> [OPTIONS]                       # Update sub-account
```

### `octos auth`

OAuth login and API key management.

```bash
octos auth login --provider openai           # PKCE browser OAuth
octos auth login --provider openai --device-code  # Device code flow
octos auth login --provider anthropic        # Paste-token (stdin)
octos auth logout --provider openai          # Remove stored credential
octos auth status                            # Show authenticated providers
```

Credentials are stored in `~/.octos/auth.json` (file mode 0600). The auth store is checked before environment variables when resolving API keys.

### `octos skills`

Manage skills.

```bash
octos skills list                            # List installed skills
octos skills install user/repo/skill-name    # Install from GitHub
octos skills remove skill-name               # Remove a skill
```

Fetches `SKILL.md` from the GitHub repo's main branch and installs to `.octos/skills/`.

---

## Working with Providers

### Supported Providers

| Provider | Env Var | Default Model | Notes |
|----------|---------|---------------|-------|
| anthropic | `ANTHROPIC_API_KEY` | claude-sonnet-4-20250514 | Native API |
| openai | `OPENAI_API_KEY` | gpt-4o | Native API |
| gemini | `GEMINI_API_KEY` | gemini-2.0-flash | Native API |
| openrouter | `OPENROUTER_API_KEY` | anthropic/claude-sonnet-4-20250514 | Aggregator |
| deepseek | `DEEPSEEK_API_KEY` | deepseek-chat | OpenAI-compatible |
| groq | `GROQ_API_KEY` | llama-3.3-70b-versatile | OpenAI-compatible |
| moonshot | `MOONSHOT_API_KEY` | kimi-k2.5 | Also: `kimi` |
| dashscope | `DASHSCOPE_API_KEY` | qwen-max | Also: `qwen` |
| minimax | `MINIMAX_API_KEY` | MiniMax-Text-01 | OpenAI-compatible |
| zhipu | `ZHIPU_API_KEY` | glm-4-plus | Also: `glm` |
| zai | `ZAI_API_KEY` | glm-5 | Z.AI, Anthropic-compatible. Also: `z.ai` |
| nvidia | `NVIDIA_API_KEY` | meta/llama-3.3-70b-instruct | NIM API. Also: `nim` |
| ollama | (none) | llama3.2 | Local, no key needed |
| vllm | `VLLM_API_KEY` | (required) | Requires --base-url |

### Provider Auto-Detection

When `--provider` is omitted, octos detects from model name:

```bash
octos chat --model gpt-4o           # -> openai
octos chat --model claude-sonnet-4-20250514  # -> anthropic
octos chat --model deepseek-chat    # -> deepseek
octos chat --model glm-4-plus       # -> zhipu
octos chat --model qwen-max         # -> dashscope
```

### Custom Endpoints

```bash
# Azure OpenAI
octos chat --provider openai --base-url "https://your.openai.azure.com/v1" --message "Task"

# Local Ollama
octos chat --provider ollama --model llama3.2

# vLLM server
octos chat --provider vllm --base-url "http://localhost:8000/v1" --model "meta-llama/Llama-3-70b" --message "Task"
```

---

## Gateway Mode

### Channel Setup

#### Telegram

```bash
export TELEGRAM_BOT_TOKEN="123456:ABC..."
```

Config:
```json
{"type": "telegram", "allowed_senders": ["your_user_id"], "settings": {"token_env": "TELEGRAM_BOT_TOKEN"}}
```

#### Slack

Requires Socket Mode app with bot token + app-level token.

```bash
export SLACK_BOT_TOKEN="xoxb-..."
export SLACK_APP_TOKEN="xapp-..."
```

Config:
```json
{"type": "slack", "settings": {"bot_token_env": "SLACK_BOT_TOKEN", "app_token_env": "SLACK_APP_TOKEN"}}
```

#### Discord

```bash
export DISCORD_BOT_TOKEN="..."
```

Config:
```json
{"type": "discord", "settings": {"token_env": "DISCORD_BOT_TOKEN"}}
```

#### WhatsApp

Requires Node.js bridge (Baileys) running at `ws://localhost:3001`.

Config:
```json
{"type": "whatsapp", "settings": {"bridge_url": "ws://localhost:3001"}}
```

#### Feishu/Lark

```bash
export FEISHU_APP_ID="cli_..."
export FEISHU_APP_SECRET="..."
```

Config:
```json
{"type": "feishu", "settings": {"app_id_env": "FEISHU_APP_ID", "app_secret_env": "FEISHU_APP_SECRET"}}
```

#### Email (IMAP/SMTP)

Requires IMAP for inbound and SMTP for outbound. Feature-gated behind `email`.

Config:
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

```bash
export EMAIL_USERNAME="bot@example.com"
export EMAIL_PASSWORD="app-specific-password"
```

#### WeCom (WeChat Work)

Requires a Custom App with message callback URL. Feature-gated behind `wecom`.

```bash
export WECOM_CORP_ID="ww..."
export WECOM_AGENT_SECRET="..."
```

Config:
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

### Voice Transcription

Voice and audio messages from channels are automatically transcribed before being sent to the agent. The system uses OminiX local ASR first (via `OMINIX_API_URL`, set automatically by `octos serve`) and falls back to Groq Whisper (cloud) when OminiX is unavailable. The transcription is prepended as `[transcription: ...]`.

```bash
# OminiX (preferred, local) — set automatically by octos serve
export OMINIX_API_URL="http://localhost:8080"

# Groq Whisper (fallback, cloud)
export GROQ_API_KEY="gsk_..."
```

### Access Control

Use `allowed_senders` to restrict access. Empty list = allow all.

```json
{"type": "telegram", "allowed_senders": ["123456", "789012"]}
```

### Cron Jobs

The agent can schedule recurring tasks via the `cron` tool:

- `add` - Create a scheduled job with `every_seconds`, `cron_expr`, or `at` timestamp
- `list` - Show all scheduled jobs
- `remove` - Delete a job by ID
- `enable` / `disable` - Toggle job active state

Cron expressions use standard syntax (e.g., `"0 0 9 * * * *"` for daily at 9am). Jobs support an optional `timezone` field with IANA timezone names (e.g., `"America/New_York"`, `"Asia/Shanghai"`). When omitted, UTC is used.

Cron jobs send messages through the bus and can deliver responses to any channel.

#### Cron CLI

Manage jobs directly from the command line (no running gateway needed):

```bash
octos cron list                          # List active jobs
octos cron list --all                    # Include disabled
octos cron add --name "report" --message "Generate daily report" --cron "0 0 9 * * * *"
octos cron add --name "check" --message "Check status" --every 3600
octos cron add --name "once" --message "Run migration" --at "2025-03-01T09:00:00Z"
octos cron remove <job-id>
octos cron enable <job-id>               # Enable
octos cron enable <job-id> --disable     # Disable
```

### Channel Status

Check configured channels and their compile/config status:

```bash
octos channels status
```

Shows a table with channel name, compile status (feature flags), and config summary (env vars set/missing).

### Heartbeat

The heartbeat service periodically reads `.octos/HEARTBEAT.md` and sends its content to the agent if non-empty. Default interval: 30 minutes. Use this for background task instructions.

---

## Memory & Skills

### Bootstrap Files

Loaded into the system prompt at startup:

| File | Purpose |
|------|---------|
| `.octos/AGENTS.md` | Agent instructions and guidelines |
| `.octos/SOUL.md` | Personality and values |
| `.octos/USER.md` | User information and preferences |
| `.octos/TOOLS.md` | Tool-specific guidance |
| `.octos/IDENTITY.md` | Custom identity definition |

Create via `octos init` (creates AGENTS, SOUL, USER templates).

### Memory System

- **Long-term**: `.octos/memory/MEMORY.md` - Persistent notes
- **Daily**: `.octos/memory/YYYY-MM-DD.md` - Auto-created daily logs
- **Recent**: Last 7 days of daily notes included in context

### Skills

Place custom skills in `.octos/skills/{name}/SKILL.md`:

```markdown
---
name: my-skill
description: My custom skill
always: true
requires_bins: curl
requires_env: MY_API_KEY
---

# Skill Instructions

Your skill content here...
```

Frontmatter fields: `name`, `description`, `always` (auto-load), `requires_bins` (comma-separated binaries checked via `which`), `requires_env` (comma-separated env vars). Availability is derived from requirement checks — not a frontmatter field.

Skills with `always: true` are included in every prompt. Others are available for the agent to read on demand.

### Built-in System Skills

octos bundles 3 system skills at compile time:

| Skill | Description | Requirements |
|-------|-------------|-------------|
| cron | Cron tool usage examples | (none, always-on) |
| skill-store | Skill installation and management | (none) |
| skill-creator | How to create custom skills | (none) |

Workspace skills in `.octos/skills/` override built-in skills with the same name.

### Bundled App-Skills

8 app-skills are compiled as separate binaries and bootstrapped into `.octos/skills/` at gateway startup:

| Skill | Binary | Description |
|-------|--------|-------------|
| news | `news_fetch` | News aggregation |
| deep-search | `deep-search` | Deep web search |
| deep-crawl | `deep_crawl` | Deep web crawling |
| send-email | `send_email` | Email sending |
| account-manager | `account_manager` | Sub-account management |
| clock | `clock` | Time and timezone queries |
| weather | `weather` | Weather information |
| asr | `asr` | Voice transcription/synthesis (platform skill, requires OminiX) |

---

### Files

```
.octos/
├── config.json          # Configuration (versioned, auto-migrated)
├── cron.json            # Cron job store
├── AGENTS.md            # Agent instructions
├── SOUL.md              # Personality
├── USER.md              # User info
├── HEARTBEAT.md         # Background tasks
├── sessions/            # Chat history (JSONL)
├── memory/              # Memory files
│   ├── MEMORY.md        # Long-term
│   └── 2025-02-10.md    # Daily
├── skills/              # Custom skills
├── episodes.redb        # Episodic memory DB
└── history/
    └── chat_history     # Readline history
```

---

## Advanced Usage

### Verbose Mode

```bash
octos chat -v                 # Shows tool execution details
```

### Tool Policies

Control which tools are available to the agent via `tools` in config:

```json
{
  "tools": {
    "allow": ["group:fs", "group:web", "shell"],
    "deny": ["spawn"]
  }
}
```

**Named groups** expand to tool sets:
- `group:fs` -> read_file, write_file, edit_file, diff_edit
- `group:runtime` -> shell
- `group:search` -> glob, grep, list_dir
- `group:web` -> web_search, web_fetch, browser
- `group:sessions` -> spawn

**Additional tools** (not in named groups): `send_file`, `switch_model`, `run_pipeline`, `configure_tool`, `cron`, `message`.

**Wildcard matching**: `exec*` matches `exec`, `exec_bg`, etc.

**Deny-wins semantics**: If a tool appears in both allow and deny, it is denied.

**Provider-specific policies**: Different tool sets per LLM model:

```json
{
  "tools": {
    "byProvider": {
      "openai/gpt-4o-mini": {
        "deny": ["shell", "write_file"]
      }
    }
  }
}
```

### Additional Config Fields

| Field | Type | Description |
|-------|------|-------------|
| `adaptive_routing` | object | Adaptive provider routing configuration |
| `voice` | object | Voice ASR/TTS configuration (auto-transcription, language) |
| `sub_providers` | array | Sub-provider configs for subagent spawning |
| `context_filter` | array of strings | Tag-based tool filtering |
| `llm_timeout_secs` | integer | LLM call timeout in seconds (gateway) |
| `tool_timeout_secs` | integer | Tool execution timeout in seconds (gateway) |

### Sandbox

Shell commands run inside a sandbox for isolation. Three backends are supported:

| Backend | Platform | Notes |
|---------|----------|-------|
| bwrap | Linux | Bubblewrap namespace isolation |
| macOS | macOS | sandbox-exec with SBPL profiles |
| Docker | Any | Container isolation with resource limits |

Configure in `config.json`:

```json
{
  "sandbox": {
    "enabled": true,
    "mode": "auto",
    "allow_network": false,
    "docker": {
      "image": "alpine:3.21",
      "mount_mode": "rw",
      "cpu_limit": "1.0",
      "memory_limit": "512m",
      "pids_limit": 100
    }
  }
}
```

**Modes**: `auto` (detect best available), `bwrap`, `macos`, `docker`, `none`.

**Mount modes**: `rw` (read-write), `ro` (read-only), `none` (no workspace mount).

**Environment sanitization**: 18 dangerous environment variables (LD_PRELOAD, NODE_OPTIONS, etc.) are automatically cleared in all sandbox backends.

### Session Forking

In gateway mode, send `/new` to create a branched conversation:

```
/new
```

This creates a new session that copies the last 10 messages from the current conversation. The child session has a `parent_key` reference to the original. Each fork gets a unique key namespaced by sender and timestamp.

### Config Hot-Reload

The gateway automatically detects config file changes:

- **Hot-reloaded** (no restart): system prompt, AGENTS.md, SOUL.md, USER.md
- **Restart required**: provider, model, API keys, gateway channels

Changes are detected via SHA-256 hashing with debounce.

### Message Coalescing

Long responses are automatically split into channel-safe chunks before sending:

| Channel | Max chars per message |
|---------|-----------------------|
| Telegram | 4000 |
| Discord | 1900 |
| Slack | 3900 |

Split preference: paragraph boundary > newline > sentence end > space > hard cut. Messages exceeding 50 chunks are truncated with a marker.

### Context Compaction

When the conversation exceeds the LLM's context window, older messages are automatically compacted:

- Tool arguments are stripped (replaced with `"[stripped]"`)
- Messages are summarized to first lines
- Recent tool call/result pairs are preserved intact
- The agent continues seamlessly without losing critical context

### Hooks (Lifecycle Events)

Run shell commands before/after tool calls and LLM calls. Configure in `config.json`:

```json
{
  "hooks": [
    {
      "event": "before_tool_call",
      "command": ["python3", "~/.octos/hooks/audit.py"],
      "timeout_ms": 3000,
      "tool_filter": ["shell", "write_file"]
    }
  ]
}
```

**Events**: `before_tool_call`, `after_tool_call`, `before_llm_call`, `after_llm_call`.

**Protocol**: Command receives JSON payload on stdin. Exit code 0 = allow, 1 = deny (before-hooks only), 2+ = error. Before-hooks can block operations; after-hooks are informational.

**Circuit breaker**: Hooks auto-disable after 3 consecutive failures. Commands use argv arrays (no shell interpretation). Environment sanitized via `BLOCKED_ENV_VARS`.

### Message Queue Modes

Control how messages arriving during an active agent run are handled:

```json
{
  "gateway": {
    "queue_mode": "followup"
  }
}
```

- **`followup`** (default): Process queued messages one at a time (FIFO)
- **`collect`**: Merge queued messages by session, concatenating content before processing

### Web UI (`octos serve`)

The REST API server (feature: `api`) includes an embedded web UI:

```bash
cargo install --path crates/octos-cli --features api
octos serve                              # Binds to 127.0.0.1:8080
octos serve --host 0.0.0.0 --port 3000  # Accept external connections
# Open http://localhost:8080
```

Features: session sidebar, chat interface, SSE streaming, dark theme. A `/metrics` endpoint provides Prometheus-format metrics (`octos_tool_calls_total`, `octos_tool_call_duration_seconds`, `octos_llm_tokens_total`).

### Hybrid Memory Search

Memory search combines BM25 (keyword) and vector (semantic) scoring:

- **Ranking**: `alpha * vector_score + (1 - alpha) * bm25_score` (default alpha: 0.7)
- **Index**: HNSW via `hnsw_rs` with L2-normalized embeddings
- **Fallback**: BM25-only when no embedding provider is configured

Configure an embedding provider to enable vector search:

```json
{
  "embedding": {
    "provider": "openai"
  }
}
```

The `EmbeddingConfig` supports three fields: `provider` (default: `"openai"`), `api_key_env` (optional override), and `base_url` (optional custom endpoint).

---

## Troubleshooting

### API Key Not Set

```
Error: ANTHROPIC_API_KEY environment variable not set
```

Fix: `export ANTHROPIC_API_KEY="your-key"` or check with `octos status`.

### Rate Limited (429)

Retry mechanism handles this automatically (3 attempts with backoff). If persistent, try a different provider or wait.

### Debug Logging

```bash
RUST_LOG=debug octos chat
RUST_LOG=octos_agent=trace octos chat --message "task"
```

### Environment Variables

| Variable | Description |
|----------|-------------|
| `ANTHROPIC_API_KEY` | Anthropic API key |
| `OPENAI_API_KEY` | OpenAI API key |
| `GEMINI_API_KEY` | Gemini API key |
| `OPENROUTER_API_KEY` | OpenRouter API key |
| `DEEPSEEK_API_KEY` | DeepSeek API key |
| `GROQ_API_KEY` | Groq API key |
| `MOONSHOT_API_KEY` | Moonshot API key |
| `DASHSCOPE_API_KEY` | DashScope API key |
| `MINIMAX_API_KEY` | MiniMax API key |
| `ZHIPU_API_KEY` | Zhipu API key |
| `ZAI_API_KEY` | Z.AI API key |
| `NVIDIA_API_KEY` | Nvidia NIM API key |
| `OMINIX_API_URL` | OminiX local ASR/TTS API URL |
| `RUST_LOG` | Log level (error/warn/info/debug/trace) |
| `TELEGRAM_BOT_TOKEN` | Telegram bot token |
| `DISCORD_BOT_TOKEN` | Discord bot token |
| `SLACK_BOT_TOKEN` | Slack bot token |
| `SLACK_APP_TOKEN` | Slack app-level token |
| `FEISHU_APP_ID` | Feishu app ID |
| `FEISHU_APP_SECRET` | Feishu app secret |
| `EMAIL_USERNAME` | Email account username |
| `EMAIL_PASSWORD` | Email account password |
| `WECOM_CORP_ID` | WeCom corp ID |
| `WECOM_AGENT_SECRET` | WeCom agent secret |
