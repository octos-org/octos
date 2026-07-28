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

  // Embedding (for vector search in memory).
  // Remote, OpenAI-compatible:
  "embedding": {
    "provider": "openai",
    "api_key_env": "OPENAI_API_KEY",
    "base_url": null,
    "model": null,       // default text-embedding-3-small (1536 dims)
    "dimensions": null   // pin the output size when the model's native size differs
  },
  // ...or in-process, no API key, any GGUF model over llama.cpp. Needs a
  // build with `--features embed-llama` (add embed-llama-metal / -cuda to
  // offload); CPU otherwise. Changing provider or model changes the vector
  // DIMENSION, which invalidates a populated index — re-embed stored
  // episodes after switching, or their recall silently degrades to BM25.
  // "embedding": {
  //   "provider": "llamacpp",
  //   "model_path": "/path/to/embeddinggemma-300M-Q8_0.gguf"
  // },

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

  // Memory injection + automatic refresh (see Memory & Skills)
  "memory": {
    "max_inject_tokens": 2500,
    "refresh": {
      "enabled": true,          // tri-state: absent = ON (default)
      "extract_model": null,    // null = profile provider
      "consolidate_model": null,
      "max_extractions_per_day": 20,
      "max_daily_tokens": 200000,
      "consolidate_interval_minutes": 30
    }
  },

  // Dashboard auth (serve mode only)
  "dashboard_auth": null,

  // Monitor (serve mode only)
  "monitor": null
}
```

> The `memory.refresh` pipeline is **on by default**. See [Memory & Skills → Automatic Memory Refresh](./memory-skills.md) for the full field list and the `octos memory` command. Opt out with `"enabled": false` or `OCTOS_MEMORY_REFRESH_ENABLED=0`.

## Runtime Tool Profiles

`octos chat` and `octos acp` resolve a **runtime profile** at startup that
decides which tools are exposed to the LLM. Every tool schema in the registry
is serialized into *every* LLM round, so the default profile is deliberately
lean.

Built-in profiles:

| Profile | Tool surface |
|---------|--------------|
| `coding` (default) | **Lean core-coding loop only**: files (`group:fs`), shell (`group:runtime`), search (`group:search`), long-term memory (`group:memory`), `spawn`, `ask_user_question` — ≈15–18 tools, ≈3.3K tokens of schemas per round. Web/research/media/messaging tools, `run_pipeline`, and bundled skills (weather, news, send_email, …) are **excluded**. |
| `coding-full` | The unfiltered pre-lean surface — every native, bundled-skill, plugin, and MCP tool (≈48 tools, ≈9K tokens per round). Byte-for-byte the old `coding` behaviour. |
| `swarm` | Coding set plus swarm-coordination tools (`send_to_agent`, `cancel_task`, `relaunch_task`, …). |

Switching profiles:

```bash
octos chat --profile coding-full        # one-off: everything back
ln -s coding-full ~/.octos/profile      # persistent default (name or path)
```

Per-project customization — drop a profile file in
`~/.octos/profiles/<name>/profile.json` (or pass a path via `--profile`) and
add tools back with allow-list entries (`group:<id>`, exact names, or
`prefix*` wildcards):

```json
{
  "name": "coding-plus-web",
  "version": 1,
  "tools": {
    "mode": "allow_list",
    "tools": ["group:fs", "group:runtime", "group:search", "group:memory",
              "spawn", "ask_user_question", "group:web"]
  },
  "agents": ["research-worker", "repo-editor"]
}
```

Notes:

- The profile filter runs **after** plugins/skills/MCP register, so it
  applies to bundled-skill and plugin tools exactly as to builtins. Tools
  marked `spawn_only` are never evicted by the filter; `run_pipeline` is
  instead gated at registration time by the profile allow list.
- Sub-agents are unaffected: `spawn` workers build their own registries, so
  the built-in `research-worker` keeps `web_search`/`web_fetch` under the
  lean default.
- `config.json`'s `tool_policy` (below) still applies and can further narrow
  (deny-wins) any profile.
- `octos gateway` / `octos serve` do not use runtime profiles; their tool
  surface is unchanged.

## Human Approval Rules

Tool calls matching a configured rule suspend the turn until an authorized
human approves or denies them on the channel (Matrix first; capable clients
like Robrix render native Approve/Deny buttons, others show a text fallback):

```json
{
  "approval_policy": {
    "default": "allow",
    "rules": [{
      "tools": ["shell", "write_file"],
      "require_approval": true,
      "risk_level": "critical",
      "authorized_approvers": ["@alice:example.org"],
      "expires_in_secs": 600,
      "on_timeout": "notify"
    }]
  }
}
```

- Rules match by exact tool name; the first matching rule wins.
- Approvals are bound to the exact tool arguments (SHA-256 digest), the
  originating room, and the `authorized_approvers` list; each request can be
  consumed once.
- `expires_in_secs` bounds how long a request stays answerable; on expiry the
  chat receives a notice (`on_timeout: "notify"`).
- Pending approvals are in-memory: a gateway restart drops them (the request
  card stays in chat but answering it reports the request as unknown).
- Decisions are appended to the JSONL audit log under `<data_dir>/audit/`
  (`OCTOS_APPROVALS_AUDIT_*` env vars control rotation/retention).
- Also available per-profile via `profile.config.approval_policy`.

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
| `DINGTALK_BOT_WEBHOOK` | DingTalk custom robot webhook URL |
| `DINGTALK_BOT_SECRET` | DingTalk robot signing secret |
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
