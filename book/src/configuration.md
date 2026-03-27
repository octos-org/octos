# Configuration

## Config File Locations

Configuration files are loaded in order (first found wins):

1. `.octos/config.json` -- project-local configuration
2. `~/.config/octos/config.json` -- global configuration

## Basic Config

A minimal configuration specifies the LLM provider and model:

```json
{
  "provider": "anthropic",
  "model": "claude-sonnet-4-20250514",
  "api_key_env": "ANTHROPIC_API_KEY"
}
```

## Gateway Config

To run Octos as a multi-channel daemon, add a `gateway` section:

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

## Environment Variable Expansion

Use `${VAR_NAME}` syntax anywhere in config values:

```json
{
  "base_url": "${ANTHROPIC_BASE_URL}",
  "model": "${OCTOS_MODEL}"
}
```

## Full Config Reference

The complete configuration structure with all available fields:

```json
{
  "version": 1,

  // LLM Provider
  "provider": "anthropic",
  "model": "claude-sonnet-4-20250514",
  "base_url": null,
  "api_key_env": null,
  "api_type": null,

  // Fallback chain
  "fallback_models": [
    {
      "provider": "deepseek",
      "model": "deepseek-chat",
      "base_url": null,
      "api_key_env": "DEEPSEEK_API_KEY"
    }
  ],

  // Adaptive routing
  "adaptive_routing": {
    "enabled": false,
    "latency_threshold_ms": 30000,
    "error_rate_threshold": 0.3,
    "probe_probability": 0.1,
    "probe_interval_secs": 60,
    "failure_threshold": 3
  },

  // Gateway
  "gateway": {
    "channels": [{"type": "cli"}],
    "max_history": 50,
    "system_prompt": null,
    "queue_mode": "followup",
    "max_sessions": 1000,
    "max_concurrent_sessions": 10,
    "llm_timeout_secs": null,
    "llm_connect_timeout_secs": null,
    "tool_timeout_secs": null,
    "session_timeout_secs": null,
    "browser_timeout_secs": null
  },

  // Tool policies
  "tool_policy": {"allow": [], "deny": []},
  "tool_policy_by_provider": {},
  "context_filter": [],

  // Sub-providers (for spawn tool)
  "sub_providers": [
    {
      "key": "cheap",
      "provider": "deepseek",
      "model": "deepseek-chat",
      "description": "Fast model for simple tasks"
    }
  ],

  // Agent settings
  "max_iterations": 50,

  // Embedding (for vector search in memory)
  "embedding": {
    "provider": "openai",
    "api_key_env": "OPENAI_API_KEY",
    "base_url": null
  },

  // Voice
  "voice": {
    "auto_asr": true,
    "auto_tts": false,
    "default_voice": "vivian",
    "asr_language": null
  },

  // Hooks
  "hooks": [],

  // MCP servers
  "mcp_servers": [],

  // Sandbox
  "sandbox": {
    "enabled": true,
    "mode": "auto",
    "allow_network": false
  },

  // Email (for email channel)
  "email": null,

  // Dashboard auth (serve mode only)
  "dashboard_auth": null,

  // Monitor (serve mode only)
  "monitor": null
}
```

## Environment Variables

### LLM Providers

| Variable | Description |
|----------|-------------|
| `ANTHROPIC_API_KEY` | Anthropic (Claude) API key |
| `OPENAI_API_KEY` | OpenAI API key |
| `GEMINI_API_KEY` | Google Gemini API key |
| `OPENROUTER_API_KEY` | OpenRouter API key |
| `DEEPSEEK_API_KEY` | DeepSeek API key |
| `GROQ_API_KEY` | Groq API key |
| `MOONSHOT_API_KEY` | Moonshot/Kimi API key |
| `DASHSCOPE_API_KEY` | Alibaba DashScope (Qwen) API key |
| `MINIMAX_API_KEY` | MiniMax API key |
| `ZHIPU_API_KEY` | Zhipu (GLM) API key |
| `ZAI_API_KEY` | Z.AI API key |
| `NVIDIA_API_KEY` | Nvidia NIM API key |

### Search

| Variable | Description |
|----------|-------------|
| `BRAVE_API_KEY` | Brave Search API key |
| `PERPLEXITY_API_KEY` | Perplexity Sonar API key |
| `YDC_API_KEY` | You.com API key |

### Channels

| Variable | Description |
|----------|-------------|
| `TELEGRAM_BOT_TOKEN` | Telegram bot token |
| `DISCORD_BOT_TOKEN` | Discord bot token |
| `SLACK_BOT_TOKEN` | Slack bot token |
| `SLACK_APP_TOKEN` | Slack app-level token |
| `FEISHU_APP_ID` | Feishu/Lark app ID |
| `FEISHU_APP_SECRET` | Feishu/Lark app secret |
| `WECOM_CORP_ID` | WeCom corp ID |
| `WECOM_AGENT_SECRET` | WeCom agent secret |
| `EMAIL_USERNAME` | Email account username |
| `EMAIL_PASSWORD` | Email account password |

### Email (send-email skill)

| Variable | Description |
|----------|-------------|
| `SMTP_HOST` | SMTP server hostname |
| `SMTP_PORT` | SMTP server port |
| `SMTP_USERNAME` | SMTP username |
| `SMTP_PASSWORD` | SMTP password |
| `SMTP_FROM` | SMTP from address |
| `LARK_APP_ID` | Feishu mail app ID |
| `LARK_APP_SECRET` | Feishu mail app secret |
| `LARK_FROM_ADDRESS` | Feishu mail from address |

### Voice

| Variable | Description |
|----------|-------------|
| `OMINIX_API_URL` | OminiX ASR/TTS API URL |

### System

| Variable | Description |
|----------|-------------|
| `RUST_LOG` | Log level (error/warn/info/debug/trace) |
| `OCTOS_LOG_JSON` | Enable JSON-formatted logs (set to any value) |

## File Layout

```
~/.octos/                        # Global config directory
├── auth.json                   # Stored API credentials (mode 0600)
├── profiles/                   # Profile configs (serve mode)
│   ├── my-bot.json
│   └── work-bot.json
├── skills/                     # Global custom skills
└── serve.log                   # Serve mode log file

.octos/                          # Project/profile data directory
├── config.json                 # Configuration
├── cron.json                   # Scheduled jobs
├── AGENTS.md                   # Agent instructions
├── SOUL.md                     # Personality definition
├── USER.md                     # User information
├── HEARTBEAT.md                # Background tasks
├── sessions/                   # Chat history (JSONL)
├── memory/                     # Memory files
│   ├── MEMORY.md               # Long-term
│   └── 2025-02-10.md           # Daily
├── skills/                     # Custom skills
├── episodes.redb               # Episodic memory DB
└── history/
    └── chat_history            # Readline history
```
