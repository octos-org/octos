# Octos User Guide

A comprehensive guide for deploying, configuring, and using the Octos AI agent platform.

---

## Table of Contents

1. [Overview](#1-overview)
2. [Dashboard & OTP Onboarding](#2-dashboard--otp-onboarding)
3. [Setting Up LLM Providers](#3-setting-up-llm-providers)
4. [Fallback & Adaptive Routing](#4-fallback--adaptive-routing)
5. [Search API Configuration](#5-search-api-configuration)
6. [Tool Configuration](#6-tool-configuration)
7. [Tool Policies](#7-tool-policies)
8. [Profile Management](#8-profile-management)
9. [Sub-Account Management](#9-sub-account-management)
10. [In-Chat Provider Management](#10-in-chat-provider-management)
11. [In-Chat Features & Commands](#11-in-chat-features--commands)
12. [Bundled App Skills](#12-bundled-app-skills)
    - [News Fetch](#121-news-fetch)
    - [Deep Search](#122-deep-search)
    - [Deep Crawl](#123-deep-crawl)
    - [Send Email](#124-send-email)
    - [Account Manager](#125-account-manager)
    - [Clock](#126-clock)
    - [Weather](#127-weather)
    - [WeChat Bridge](#128-wechat-bridge)
    - [Pipeline Guard](#129-pipeline-guard)
13. [Platform Skills (ASR/TTS)](#13-platform-skills-asrtts)
14. [Custom Skill Installation](#14-custom-skill-installation)
15. [Configuration Reference](#15-configuration-reference)
16. [Matrix Appservice (Palpo)](#16-matrix-appservice-palpo)

---

## 1. Overview

Octos is a Rust-native AI agent platform that runs in two modes:

- **`octos serve`** — Control plane with admin dashboard. Manages multiple **profiles** (bot instances), each running as an isolated gateway child process with its own config, memory, sessions, and messaging channels.
- **`octos gateway`** — A single gateway instance serving messaging channels (Telegram, Discord, Slack, WhatsApp, Feishu, Email, WeCom, Matrix).
- **`octos chat`** — Interactive CLI chat for development and testing.

### Architecture

```
octos serve (control plane + dashboard)
  ├── Profile A → gateway process (Telegram, WhatsApp)
  ├── Profile B → gateway process (Feishu, Slack)
  └── Profile C → gateway process (CLI)
       │
       ├── LLM Provider (kimi-2.5, deepseek-chat, gpt-4o, etc.)
       ├── Tool Registry (shell, files, search, web, skills...)
       ├── Session Store (per-channel conversation history)
       ├── Memory (MEMORY.md, daily notes, episodes)
       └── Skills (bundled + custom)
```

Each profile is fully isolated — its own data directory, memory, sessions, skills, and API keys. Sub-accounts can be created under a profile and inherit the parent's LLM configuration.

---

## 2. Dashboard & OTP Onboarding

The admin dashboard is a React web application embedded in the `octos serve` binary. It provides a visual interface for managing profiles, monitoring gateway status, and configuring the system.

### 2.1 Accessing the Dashboard

```bash
# Start the control plane
octos serve --host 0.0.0.0 --port 3000

# Dashboard is available at:
# http://localhost:3000
```

If you're running behind a reverse proxy (e.g., Caddy or Nginx), configure it to forward to the serve port.

Deployment behavior depends on `config.mode`:

- `local` — Standalone machine. `/` redirects to `/admin/`.
- `tenant` — Default end-user machine setup. Direct installs stay local at `/admin/`; managed registration setup can also configure the machine's public tunnel.
- `cloud` — Advanced relay-host setup. `/` serves the landing page and `/admin/` remains the admin dashboard.

`~/.octos/config.json` is the file that `octos serve` reads at startup. Tenant and local installs create it through the normal installers; host installs can now bootstrap it with `scripts/cloud-host-deploy.sh`, which writes `mode: "cloud"` plus the relay settings used by the landing page and frps plugin.

### 2.1.1 Cloud Host Bootstrap

For the relay/host VPS itself, use:

```bash
bash scripts/cloud-host-deploy.sh
```

That script:

- creates or updates `~/.octos/config.json` for cloud mode
- installs `octos serve`
- runs `scripts/frp/setup-frps.sh`
- runs `scripts/frp/setup-caddy.sh`
- saves rerun settings to `~/.octos/cloud-bootstrap.env`

For unattended setup, pass `--config <env-file> --non-interactive`.

**Per-tenant frps authentication.** Tenants no longer share a single FRPS auth token. Each tenant gets its own `tunnel_token` (a UUID, generated at registration time) that the frpc client sends in `metadatas.token`; `frps` forwards Login and NewProxy operations to an octos plugin endpoint that validates the token against the tenant store and caches the `run_id → tenant_id` mapping for subsequent proxy requests. Both `frps` and `frpc` are configured with `auth.token = ""` — the built-in token check is a no-op and all tenant identity rides in the metadata field.

### 2.1.2 Tenant Bootstrap

End users register themselves via the cloud host's public signup page (e.g., `https://octos.example.com`) and receive a personalized setup command covering macOS/Linux and Windows. The command embeds the tenant's subdomain, per-tenant tunnel token, SSH port, and dashboard auth token, so no values need to be typed manually.

A typical emitted command (macOS/Linux):

```bash
curl -fsSL https://github.com/octos-org/octos/releases/latest/download/install.sh | bash -s -- \
    --tunnel \
    --tenant-name alice \
    --frps-token <per-tenant-uuid> \
    --ssh-port 6001 \
    --domain octos.example.com \
    --frps-server frps.octos.example.com \
    --auth-token <dashboard-token>
```

The installer writes `/etc/frp/frpc.toml` with the per-tenant UUID under `metadatas.token`, brings frpc up as a launchd/systemd service, and starts `octos serve` on the configured local port. On reruns (`~/.octos/bin/install.sh --tunnel`), the installer recovers the token from the existing `metadatas.token` entry instead of prompting.

### 2.1.3 Uninstall

To remove an installation:

| Machine | Command |
|---------|---------|
| Tenant (macOS/Linux) | `~/.octos/bin/install.sh --uninstall` |
| Tenant (Windows)     | `& "$HOME\.octos\bin\install.ps1" -Uninstall` |
| Cloud VPS (services) | `bash scripts/cloud-host-deploy.sh --uninstall` |
| Cloud VPS (+ data)   | `bash scripts/cloud-host-deploy.sh --uninstall --purge` |

On a tenant, the uninstall flag stops and removes both `octos-serve` and `frpc` services, deletes `/etc/frp` and `/usr/local/bin/frpc`, stops Caddy if present, and (on Linux) removes the firewall rules it added. The data directory (`~/.octos`) is always preserved unless you delete it manually.

On the cloud VPS, `cloud-host-deploy.sh --uninstall` calls `install.sh --uninstall` internally and additionally stops and removes `frps.service` and the Caddy host configuration. Using plain `install.sh --uninstall` on the VPS is not recommended because it removes `/etc/frp` and `/usr/local/bin/frpc` without stopping `frps.service`.

### 2.2 OTP Email Authentication

The dashboard uses email-based One-Time Password (OTP) authentication. No passwords are stored — a 6-digit code is emailed to the user each time they log in.

#### Configure SMTP for OTP Emails

Add `dashboard_auth` to your serve config (`~/.octos/config.json` or `<cwd>/.octos/config.json`):

```json
{
  "dashboard_auth": {
    "smtp": {
      "host": "smtp.gmail.com",
      "port": 465,
      "username": "your-email@gmail.com",
      "password_env": "SMTP_PASSWORD",
      "from_address": "your-email@gmail.com"
    },
    "session_expiry_hours": 24,
    "allow_self_registration": false
  }
}
```

- **`host`** — SMTP server (e.g., `smtp.gmail.com`, `smtp.office365.com`)
- **`port`** — 465 for implicit TLS, 587 for STARTTLS
- **`username`** — SMTP login username
- **`password_env`** — Environment variable name holding the SMTP password (e.g., `SMTP_PASSWORD`). For Gmail, use an [App Password](https://support.google.com/accounts/answer/185833).
- **`from_address`** — The "From" address on OTP emails
- **`session_expiry_hours`** — How long a login session lasts (default: 24 hours)
- **`allow_self_registration`** — If `false`, only pre-registered users (created via admin API) can log in

Set the SMTP password environment variable before starting:

```bash
export SMTP_PASSWORD="your-app-password"
```

#### Login Flow

1. Open the dashboard in your browser
2. Enter your email address on the login page
3. Check your email for the 6-digit OTP code
4. Enter the code on the verification page
5. You're logged in for the configured session duration

**Security details:**
- One OTP per email per 60 seconds (rate-limited)
- OTP expires after 5 minutes
- 3 wrong attempts invalidate the OTP
- Session token: 64-character hex string (32 bytes of randomness)
- Constant-time comparison prevents timing attacks
- If `allow_self_registration` is disabled and the email isn't registered, no email is sent (but the server returns success to prevent email enumeration)

**Dev mode:** If no SMTP is configured, the OTP code is printed to the server console log instead of emailed. This is useful for local development.

### 2.3 Dashboard Features

Once logged in, the dashboard provides:

- **Overview** — Total profiles, running/stopped counts, quick status of all bots
- **Profile Management** — Create, edit, start, stop, restart, and delete profiles
- **Log Viewer** — Real-time SSE log streaming for each gateway process
- **Provider Testing** — Test LLM provider/model/API key combinations before deploying
- **WhatsApp QR** — Scan QR code to link a WhatsApp number
- **Platform Skills** — Monitor and manage OminiX ASR/TTS services
- **Metrics** — Per-profile LLM provider QoS metrics (latency, error rates)

---

## 3. Setting Up LLM Providers

Octos supports 14 LLM providers out of the box. Each provider requires an API key set as an environment variable.

### 3.1 Supported Providers

| Provider | Env Variable | Default Model | API Format | Aliases |
|----------|-------------|---------------|------------|---------|
| `anthropic` | `ANTHROPIC_API_KEY` | claude-sonnet-4-20250514 | Native Anthropic | — |
| `openai` | `OPENAI_API_KEY` | gpt-4o | Native OpenAI | — |
| `gemini` | `GEMINI_API_KEY` | gemini-2.0-flash | Native Gemini | — |
| `openrouter` | `OPENROUTER_API_KEY` | anthropic/claude-sonnet-4-20250514 | Native OpenRouter | — |
| `deepseek` | `DEEPSEEK_API_KEY` | deepseek-chat | OpenAI-compatible | — |
| `groq` | `GROQ_API_KEY` | llama-3.3-70b-versatile | OpenAI-compatible | — |
| `moonshot` | `MOONSHOT_API_KEY` | kimi-k2.5 | OpenAI-compatible | `kimi` |
| `dashscope` | `DASHSCOPE_API_KEY` | qwen-max | OpenAI-compatible | `qwen` |
| `minimax` | `MINIMAX_API_KEY` | MiniMax-Text-01 | OpenAI-compatible | — |
| `zhipu` | `ZHIPU_API_KEY` | glm-4-plus | OpenAI-compatible | `glm` |
| `zai` | `ZAI_API_KEY` | glm-5 | Anthropic-compatible | `z.ai` |
| `nvidia` | `NVIDIA_API_KEY` | meta/llama-3.3-70b-instruct | OpenAI-compatible | `nim` |
| `ollama` | *(none)* | llama3.2 | OpenAI-compatible | — |
| `vllm` | `VLLM_API_KEY` | *(must specify)* | OpenAI-compatible | — |

#### How to Get API Keys

**Google Gemini:**
1. Go to [Google AI Studio](https://aistudio.google.com/apikey)
2. Sign in with your Google account
3. Click "Create API Key" and select or create a Google Cloud project
4. Copy the generated API key
5. Set it: `export GEMINI_API_KEY="your-key"`

**Alibaba DashScope (Qwen):**
1. Go to [DashScope Console](https://dashscope.console.aliyun.com/)
2. Sign up or log in with an Alibaba Cloud account
3. Navigate to **API-KEY Management** (API-KEY 管理)
4. Click "Create API Key" (创建新的 API-KEY)
5. Copy the generated key
6. Set it: `export DASHSCOPE_API_KEY="your-key"`

**DeepSeek:**
1. Go to [DeepSeek Platform](https://platform.deepseek.com/api_keys)
2. Sign up or log in
3. Click "Create new API key"
4. Copy the key
5. Set it: `export DEEPSEEK_API_KEY="your-key"`

**Moonshot / Kimi:**
1. Go to [Moonshot Platform](https://platform.moonshot.cn/console/api-keys)
2. Sign up or log in
3. Click "Create new API key" (新建 API Key)
4. Copy the key
5. Set it: `export MOONSHOT_API_KEY="your-key"`

**OpenAI:**
1. Go to [OpenAI API Keys](https://platform.openai.com/api-keys)
2. Sign up or log in
3. Click "Create new secret key"
4. Copy the key
5. Set it: `export OPENAI_API_KEY="your-key"`

**Anthropic:**
1. Go to [Anthropic Console](https://console.anthropic.com/settings/keys)
2. Sign up or log in
3. Click "Create Key"
4. Copy the key
5. Set it: `export ANTHROPIC_API_KEY="your-key"`

**MiniMax:**
1. Go to [MiniMax Open Platform](https://platform.minimaxi.com/)
2. Sign up or log in
3. Navigate to **API Keys** in the console
4. Click "Create API Key"
5. Copy the key
6. Set it: `export MINIMAX_API_KEY="your-key"`

**Z.AI:**
1. Go to [Z.AI Platform](https://z.ai/)
2. Sign up or log in
3. Navigate to the API key management page
4. Create a new API key
5. Copy the key
6. Set it: `export ZAI_API_KEY="your-key"`
7. Note: Z.AI uses the Anthropic Messages API protocol (`api_type: "anthropic"`)

**Nvidia NIM:**
1. Go to [Nvidia NIM](https://build.nvidia.com/)
2. Sign up or log in with your Nvidia account
3. Navigate to any model page and click "Get API Key"
4. Copy the generated key
5. Set it: `export NVIDIA_API_KEY="your-key"`
6. Note: Nvidia NIM hosts many models — you must specify the model name explicitly (e.g., `meta/llama-3.3-70b-instruct`)

**OpenRouter:**
1. Go to [OpenRouter](https://openrouter.ai/keys)
2. Sign up or log in
3. Click "Create Key"
4. Copy the key
5. Set it: `export OPENROUTER_API_KEY="your-key"`
6. Note: OpenRouter is a multi-model aggregator — use model names like `anthropic/claude-sonnet-4-20250514`, `openai/gpt-4o`, etc.

### 3.2 Configuration Methods

#### Method 1: Config File

Set `provider` and `model` in your config:

```json
{
  "provider": "moonshot",
  "model": "kimi-2.5",
  "api_key_env": "KIMI_API_KEY"
}
```

The `api_key_env` field overrides the default env variable name for the provider. For example, Moonshot's default is `MOONSHOT_API_KEY`, but you can use `KIMI_API_KEY` instead.

#### Method 2: CLI Flags

```bash
octos chat --provider deepseek --model deepseek-chat
octos chat --model gpt-4o  # auto-detects provider from model name
```

#### Method 3: Auto-Detection

When `provider` is omitted, Octos detects the provider from the model name:

| Model Pattern | Detected Provider |
|--------------|-------------------|
| `claude-*` | anthropic |
| `gpt-*`, `o1-*`, `o3-*`, `o4-*` | openai |
| `gemini-*` | gemini |
| `deepseek-*` | deepseek |
| `kimi-*`, `moonshot-*` | moonshot |
| `qwen-*` | dashscope |
| `glm-*` | zhipu |
| `llama-*` | groq |

### 3.3 Custom Endpoints

Use `base_url` to point to self-hosted or proxy endpoints:

```json
{
  "provider": "openai",
  "model": "gpt-4o",
  "base_url": "https://your-azure-endpoint.openai.azure.com/v1"
}
```

```json
{
  "provider": "ollama",
  "model": "llama3.2",
  "base_url": "http://localhost:11434/v1"
}
```

```json
{
  "provider": "vllm",
  "model": "meta-llama/Llama-3-70b",
  "base_url": "http://localhost:8000/v1"
}
```

### 3.4 API Type Override

The `api_type` field forces a specific API wire format:

```json
{
  "provider": "zai",
  "model": "glm-5",
  "api_type": "anthropic"
}
```

- `"openai"` — OpenAI Chat Completions format (default for most providers)
- `"anthropic"` — Anthropic Messages format (for Anthropic-compatible proxies like Z.AI)

### 3.5 Auth Store (OAuth & Paste-Token)

Instead of environment variables, you can store API keys via the auth CLI:

```bash
# OAuth PKCE (OpenAI only)
octos auth login --provider openai

# Device code flow (OpenAI only)
octos auth login --provider openai --device-code

# Paste-token (all other providers)
octos auth login --provider anthropic
# → prompts: "Paste your API key:"

# Check stored credentials
octos auth status

# Remove credentials
octos auth logout --provider openai
```

Credentials are stored in `~/.octos/auth.json` (file mode 0600). The auth store is checked **before** environment variables when resolving API keys.

---

## 4. Fallback & Adaptive Routing

### 4.1 Static Fallback Chain

Configure a priority-ordered fallback chain. If the primary provider fails (401, 403, rate limit, 5xx), the next provider in the chain is tried:

```json
{
  "provider": "moonshot",
  "model": "kimi-2.5",
  "fallback_models": [
    {
      "provider": "deepseek",
      "model": "deepseek-chat",
      "api_key_env": "DEEPSEEK_API_KEY"
    },
    {
      "provider": "gemini",
      "model": "gemini-2.0-flash",
      "api_key_env": "GEMINI_API_KEY"
    }
  ]
}
```

**Failover rules:**
- 401/403 (authentication errors) → failover immediately (don't retry the same provider)
- 429 (rate limit) / 5xx (server errors) → retry with exponential backoff, then failover
- Circuit breaker: 3 consecutive failures → provider marked degraded

### 4.2 Adaptive Routing

When multiple fallback models are configured, enable adaptive routing to dynamically select the best provider based on real-time metrics:

```json
{
  "adaptive_routing": {
    "enabled": true,
    "latency_threshold_ms": 30000,
    "error_rate_threshold": 0.3,
    "probe_probability": 0.1,
    "probe_interval_secs": 60,
    "failure_threshold": 3
  }
}
```

- **`latency_threshold_ms`** — Providers with average latency above this are penalized (default: 30s)
- **`error_rate_threshold`** — Providers with error rates above this are deprioritized (default: 30%)
- **`probe_probability`** — Fraction of requests sent to non-primary providers as health probes (default: 10%)
- **`probe_interval_secs`** — Minimum time between probes to the same provider (default: 60s)
- **`failure_threshold`** — Consecutive failures before circuit breaker opens (default: 3)

When adaptive routing is enabled, it replaces the static priority chain with dynamic selection based on latency and error rate metrics.

---

## 5. Search API Configuration

The `web_search` tool uses multiple search providers with automatic fallback.

### 5.1 Supported Search Providers

| Provider | Env Variable | Cost | Notes |
|----------|-------------|------|-------|
| DuckDuckGo | *(none)* | Free | Always available, HTML scraping fallback |
| Brave Search | `BRAVE_API_KEY` | Free tier: 2K queries/month | REST API |
| You.com | `YDC_API_KEY` | Paid | Rich JSON results with snippets |
| Perplexity Sonar | `PERPLEXITY_API_KEY` | Paid | AI-synthesized answers with citations |

### 5.2 Provider Selection

Providers are tried in order: **DuckDuckGo → Brave → You.com → Perplexity**. The first provider returning non-empty results wins. If all fail, DuckDuckGo results are returned as fallback.

To use a specific provider, set its API key:

```bash
export BRAVE_API_KEY="your-brave-key"
# or
export PERPLEXITY_API_KEY="pplx-your-key"
```

### 5.3 Configuring Default Result Count

```
/config set web_search.count 10
```

This persists across sessions and applies to all searches unless the caller explicitly provides a `count`.

### 5.4 Sample Chat Usage

```
User: Search for the latest Rust 1.85 release notes

Bot: [uses web_search tool with query "Rust 1.85 release notes"]
     Here's a summary of what's new in Rust 1.85...
```

---

## 6. Tool Configuration

Tools can be configured at runtime using the `/config` slash command. Settings persist in `{data_dir}/tool_config.json`.

### 6.1 Configurable Tools

| Tool | Setting | Type | Default | Description |
|------|---------|------|---------|-------------|
| `news_digest` | `language` | `"zh"` / `"en"` | `"zh"` | Output language for news digests |
| `news_digest` | `hn_top_stories` | 5-100 | 30 | Hacker News stories to fetch |
| `news_digest` | `max_rss_items` | 5-100 | 30 | Items per RSS feed |
| `news_digest` | `max_deep_fetch_total` | 1-50 | 20 | Total articles to deep-fetch |
| `news_digest` | `max_source_chars` | 1000-50000 | 12000 | Per-source HTML char limit |
| `news_digest` | `max_article_chars` | 1000-50000 | 8000 | Per-article content limit |
| `deep_crawl` | `page_settle_ms` | 500-10000 | 3000 | JS render wait time (ms) |
| `deep_crawl` | `max_output_chars` | 10000-200000 | 50000 | Output truncation limit |
| `web_search` | `count` | 1-10 | 5 | Default number of search results |
| `web_fetch` | `extract_mode` | `"markdown"` / `"text"` | `"markdown"` | Content extraction format |
| `web_fetch` | `max_chars` | 1000-200000 | 50000 | Content size limit |
| `browser` | `action_timeout_secs` | 30-600 | 300 | Per-action timeout |
| `browser` | `idle_timeout_secs` | 60-600 | 300 | Idle session timeout |

### 6.2 In-Chat Config Commands

```
/config                              # Show all tool settings
/config web_search                   # Show web_search settings
/config set web_search.count 10      # Set default result count to 10
/config set news_digest.language en  # Switch news digests to English
/config reset web_search.count       # Reset to default (5)
```

### 6.3 Priority Order

Setting values are resolved in this order (highest priority first):
1. Explicit per-call arguments (tool invocation parameters)
2. `/config` overrides (stored in `tool_config.json`)
3. Hardcoded defaults

---

## 7. Tool Policies

Tool policies control which tools the agent can use. They can be set globally, per-provider, or per-context.

### 7.1 Global Policy

```json
{
  "tool_policy": {
    "allow": ["group:fs", "group:search", "web_search"],
    "deny": ["shell", "spawn"]
  }
}
```

- **`allow`** — If non-empty, only these tools are permitted. If empty, all tools are allowed.
- **`deny`** — These tools are always blocked. **Deny wins over allow.**

### 7.2 Named Groups

| Group | Expands To |
|-------|-----------|
| `group:fs` | `read_file`, `write_file`, `edit_file`, `diff_edit` |
| `group:runtime` | `shell` |
| `group:web` | `web_search`, `web_fetch`, `browser` |
| `group:search` | `glob`, `grep`, `list_dir` |
| `group:sessions` | `spawn` |

### 7.3 Wildcard Matching

Suffix `*` matches prefixes:

```json
{
  "tool_policy": {
    "deny": ["web_*"]
  }
}
```

This denies `web_search`, `web_fetch`, etc.

### 7.4 Per-Provider Policies

Different tool sets for different LLM models:

```json
{
  "tool_policy_by_provider": {
    "openai/gpt-4o-mini": {
      "deny": ["shell", "write_file"]
    },
    "gemini": {
      "deny": ["diff_edit"]
    }
  }
}
```

Model-specific keys (e.g., `openai/gpt-4o-mini`) take priority over provider-level keys (e.g., `gemini`).

### 7.5 Tag-Based Filtering

Use `context_filter` to restrict tools to specific tags:

```json
{
  "context_filter": ["gateway"]
}
```

Only tools tagged with at least one matching tag are available. Tools with no tags always pass (they are "universal").

---

## 8. Profile Management

Profiles are bot instances managed through the admin dashboard or API. Each profile has its own configuration, data directory, and gateway process.

### 8.1 Creating a Profile

#### Via Dashboard

1. Click "New Profile" on the dashboard
2. Fill in: ID (slug), display name, provider, model, API key env var
3. Add channels (Telegram token, WhatsApp bridge URL, etc.)
4. Set system prompt
5. Click "Create"

#### Via Admin API

```bash
curl -X POST http://localhost:3000/api/admin/profiles \
  -H "Content-Type: application/json" \
  -d '{
    "id": "my-bot",
    "name": "My Bot",
    "enabled": false,
    "config": {
      "provider": "moonshot",
      "model": "kimi-2.5",
      "api_key_env": "KIMI_API_KEY",
      "gateway": {
        "channels": [
          {"type": "telegram", "allowed_senders": ["123456789"]}
        ],
        "system_prompt": "You are a helpful assistant."
      }
    }
  }'
```

### 8.2 Profile Lifecycle (Start / Stop / Restart)

#### Via Dashboard

Use the Start / Stop / Restart buttons on each profile card.

#### Via Admin API

```bash
# Start a profile's gateway
curl -X POST http://localhost:3000/api/admin/profiles/my-bot/start

# Stop a profile's gateway
curl -X POST http://localhost:3000/api/admin/profiles/my-bot/stop

# Restart (stop + start)
curl -X POST http://localhost:3000/api/admin/profiles/my-bot/restart

# Check status
curl http://localhost:3000/api/admin/profiles/my-bot/status
```

**Start validation:** The start endpoint validates that an LLM provider is configured before launching the gateway. If the provider or API key is missing, it returns an error.

### 8.3 Updating a Profile

Updates use **JSON merge** — only the fields you include are modified. All other fields are preserved.

```bash
curl -X PUT http://localhost:3000/api/admin/profiles/my-bot \
  -H "Content-Type: application/json" \
  -d '{
    "name": "Updated Bot Name",
    "config": {
      "model": "kimi-k2.5",
      "fallback_models": [
        {"provider": "deepseek", "model": "deepseek-chat"}
      ]
    }
  }'
```

### 8.4 Deleting a Profile

```bash
curl -X DELETE http://localhost:3000/api/admin/profiles/my-bot
```

This stops the gateway process (if running) and cascades to all sub-accounts.

### 8.5 Viewing Logs

```bash
# SSE log stream (real-time)
curl http://localhost:3000/api/admin/profiles/my-bot/logs

# Provider metrics
curl http://localhost:3000/api/admin/profiles/my-bot/metrics
```

### 8.6 API Overview Endpoint

```bash
# Get summary of all profiles
curl http://localhost:3000/api/admin/overview
```

Returns total count, running/stopped counts, and status of each profile.

### 8.7 Testing a Provider

Before deploying, test a provider configuration:

```bash
curl -X POST http://localhost:3000/api/admin/test-provider \
  -H "Content-Type: application/json" \
  -d '{
    "provider": "moonshot",
    "model": "kimi-2.5",
    "api_key_env": "KIMI_API_KEY"
  }'
```

Returns success/failure and the provider's response.

---

## 9. Sub-Account Management

Sub-accounts are child bot instances that inherit their parent profile's LLM provider configuration but have their own data directory (memory, sessions, skills) and messaging channels.

### 9.1 How Sub-Accounts Work

- **Inherits from parent:** LLM provider, model, API keys, fallback chain
- **Has its own:** Data directory, sessions, memory, skills, system prompt, channels
- **ID format:** `{parent-id}--{slug}` (e.g., `dspfac--work-bot`)
- **Managed via:** The `manage_account` tool (available in-chat) or the admin API

### 9.2 In-Chat Sub-Account Management

The `account-manager` bundled skill provides the `manage_account` tool. Users can manage sub-accounts by chatting with their bot naturally:

#### List Sub-Accounts

```
User: Show me all my sub-accounts

Bot: [uses manage_account tool with action="list"]
     Here are your sub-accounts:
     1. work-bot (dspfac--work-bot) - Running
     2. news-bot (dspfac--news-bot) - Stopped
```

#### Create a Sub-Account

```
User: Create a new sub-account called "coding assistant" with the system prompt
      "You are a coding expert specializing in Rust and Python."
      Connect it to Telegram with token 123:ABC...

Bot: [uses manage_account tool with action="create"]
     Created sub-account "coding assistant" (dspfac--coding-assistant).
     Telegram channel configured. Starting...
```

#### Start / Stop / Restart

```
User: Stop the work-bot sub-account
Bot: [uses manage_account with action="stop", sub_account_id="dspfac--work-bot"]
     Stopped work-bot.

User: Restart the news-bot
Bot: [uses manage_account with action="restart", sub_account_id="dspfac--news-bot"]
     Restarted news-bot.
```

#### Update Configuration

```
User: Update my work-bot to only allow messages from Telegram user 5460262597

Bot: [uses manage_account with action="update", sub_account_id="dspfac--work-bot",
      telegram_senders="5460262597"]
     Updated work-bot: Telegram sender whitelist set to [5460262597].
```

#### Delete a Sub-Account

```
User: Delete the old-bot sub-account

Bot: [uses manage_account with action="delete", sub_account_id="dspfac--old-bot"]
     Deleted sub-account old-bot and all its data.
```

#### Get Sub-Account Info

```
User: Show me details about the work-bot

Bot: [uses manage_account with action="info", sub_account_id="dspfac--work-bot"]
     work-bot (dspfac--work-bot):
     - Status: Running
     - Channels: Telegram (allowed: 5460262597)
     - System prompt: "You are a work assistant."
```

---

## 10. In-Chat Provider Management

The `switch_model` tool allows users to list available LLM providers and switch models at runtime through natural conversation. This tool is only available in gateway mode.

### 10.1 List Available Providers

```
User: What models are available?

Bot: [uses switch_model tool with action="list"]
     Current model: moonshot/kimi-2.5

     Available providers:
       - anthropic (default: claude-sonnet-4-20250514) [ready]
       - openai (default: gpt-4o) [ready]
       - deepseek (default: deepseek-chat) [ready]
       - gemini (default: gemini-2.0-flash) [ready]
       - moonshot (default: kimi-k2.5) [ready] [aliases: kimi]
       - ollama (default: llama3.2) [no key needed]
       ...

     Configured fallback models:
       - deepseek/deepseek-chat
```

### 10.2 Switch Models

```
User: Switch to deepseek-chat

Bot: [uses switch_model tool with action="switch", model="deepseek-chat"]
     Switched to deepseek/deepseek-chat.
     Previous model (moonshot/kimi-2.5) is kept as fallback.
```

```
User: Use GPT-4o instead

Bot: [uses switch_model tool with action="switch", model="gpt-4o"]
     Switched to openai/gpt-4o.
     Previous model (moonshot/kimi-2.5) is kept as fallback.
```

### 10.3 How Fallback Works

When you switch models, the previous model automatically becomes a fallback:
- If the new model fails (rate limit, server error), requests automatically fall back to the original model
- The fallback uses the existing `ProviderChain` circuit breaker (3 consecutive failures triggers failover)
- The chain is always flat: `[new_model, original_model]` — repeated switches don't nest

### 10.4 Custom Provider Settings

```
User: Switch to a model at my custom endpoint

Bot: [uses switch_model with action="switch", model="my-model",
      provider="openai", base_url="https://my-endpoint.com/v1",
      api_key_env="MY_API_KEY"]
     Switched to openai/my-model.
```

### 10.5 Persistence

Model switches are persisted to the profile JSON file. On gateway restart, the bot starts with the last-selected model.

---

## 11. In-Chat Features & Commands

### 11.1 Slash Commands

| Command | Description |
|---------|-------------|
| `/new` | Fork the conversation — creates a new session copying the last 10 messages |
| `/config` | View and modify tool configuration (see [Section 6](#6-tool-configuration)) |
| `/exit`, `/quit`, `:q` | Exit chat (CLI mode only) |

### 11.2 Session Management

Each channel:chat_id pair maintains its own session (conversation history).

- **Session persistence:** JSONL files in `.octos/sessions/`
- **Max history:** Configurable via `gateway.max_history` (default: 50 messages)
- **Session forking:** `/new` creates a branched conversation with parent_key tracking
- **Context compaction:** When conversation exceeds the LLM's context window, older messages are automatically compacted (tool arguments stripped, early messages summarized)

### 11.3 Memory System

The agent maintains long-term memory across sessions:

- **`MEMORY.md`** — Persistent notes, always loaded into context
- **Daily notes** — `.octos/memory/YYYY-MM-DD.md`, auto-created
- **Recent memory** — Last 7 days of daily notes included in context
- **Episodes** — Task completion summaries stored in `episodes.redb`

```
User: Remember that I prefer Python for scripts and Rust for systems work.

Bot: I've noted that in memory.
     [writes to MEMORY.md]
```

### 11.4 Cron Jobs (Scheduled Tasks)

The agent can schedule recurring tasks using the `cron` tool:

```
User: Schedule a daily news digest at 8am Beijing time

Bot: [uses cron tool]
     Created cron job "daily-news" running at 8:00 AM Asia/Shanghai every day.
     Expression: 0 0 8 * * * *
```

```
User: Show my scheduled jobs

Bot: [uses cron tool with action="list"]
     Active cron jobs:
     1. daily-news — "Generate news digest" — 0 0 8 * * * * (Asia/Shanghai) — enabled
```

Cron jobs can also be managed via CLI:

```bash
octos cron list                              # List active jobs
octos cron list --all                        # Include disabled
octos cron add --name "report" --message "Generate daily report" --cron "0 0 9 * * * *"
octos cron add --name "check" --message "Check status" --every 3600
octos cron remove <job-id>
octos cron enable <job-id>
octos cron enable <job-id> --disable
```

### 11.5 Multi-Turn Tool Use

The agent can use multiple tools in sequence within a single response:

```
User: Find all Python files in the project, then search for TODO comments

Bot: [uses glob tool to find *.py files]
     [uses grep tool to search for TODO]
     Found 12 Python files with 5 TODO comments:
     - src/main.py:42: # TODO: add error handling
     ...
```

### 11.6 File Operations

```
User: Read the config file at /etc/nginx/nginx.conf

Bot: [uses read_file tool]
     Here's the contents of nginx.conf:
     ...
```

```
User: Create a new Python script that fetches weather data

Bot: [uses write_file tool]
     Created weather.py with the following contents...
```

### 11.7 Shell Commands

```
User: Run the test suite

Bot: [uses shell tool: cargo test --workspace]
     All 464 tests passed.
```

### 11.8 Web Browsing

```
User: Open https://example.com and take a screenshot

Bot: [uses browser tool to navigate and screenshot]
     Here's the screenshot of example.com...
```

### 11.9 Spawning Sub-Agents

```
User: Research this topic in depth using a sub-agent

Bot: [uses spawn tool to create a sub-agent with the research task]
     The sub-agent found the following...
```

Sub-agents can use different LLM models via `sub_providers`:

```json
{
  "sub_providers": [
    {
      "key": "cheap",
      "provider": "deepseek",
      "model": "deepseek-chat",
      "description": "Fast model for simple tasks"
    }
  ]
}
```

### 11.10 Message Queue Modes

When a user sends messages while the agent is processing:

- **`followup`** (default): Queued messages are processed one at a time (FIFO)
- **`collect`**: Messages from the same session are concatenated and processed as one

```json
{
  "gateway": {
    "queue_mode": "collect"
  }
}
```

### 11.11 Heartbeat

The heartbeat service reads `.octos/HEARTBEAT.md` every 30 minutes and sends its content to the agent. Use it for background task instructions:

```markdown
<!-- .octos/HEARTBEAT.md -->
Check for new issues in the GitHub repo and summarize any urgent ones.
```

---

## 12. Bundled App Skills

Bundled app skills ship as compiled binaries alongside the `octos` binary. They are automatically bootstrapped into `.octos/skills/` on gateway startup — no installation required.

### 12.1 News Fetch

**Tool name:** `news_fetch`
**Always active:** Yes (automatically included in every conversation)

Fetches raw news headlines and full article content from Google News RSS, Hacker News API, Yahoo News, Substack, and Medium. The tool returns raw data — the agent synthesizes it into a formatted digest.

#### Parameters

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `categories` | array of strings | all | News categories to fetch |
| `language` | `"zh"` / `"en"` | `"zh"` | Language for the output digest |

**Available categories:** `politics`, `world` / `international`, `business` / `commerce`, `technology` / `tech`, `science`, `entertainment` / `social`, `health`, `sports`

#### Sample Chat Usage

```
User: Give me today's tech and world news in English

Bot: [uses news_fetch with categories=["tech", "world"], language="en"]
     📰 Tech News:
     1. AI startup raises $500M in Series C funding...
     2. New quantum computing breakthrough at MIT...

     🌍 World News:
     1. EU passes new digital regulations...
     ...
```

```
User: 请生成今日新闻速递

Bot: [uses news_fetch with language="zh"]
     📰 今日新闻速递

     🔬 科技:
     1. OpenAI发布新模型...

     💼 商业:
     ...
```

#### Cron Scheduling

```
User: Schedule a daily news digest at 8am Shanghai time

Bot: [creates cron job]
     Done! I'll send a news digest every morning at 8:00 AM (Asia/Shanghai).
```

#### Configuration

```
/config set news_digest.language en          # English output
/config set news_digest.hn_top_stories 50    # Fetch 50 HN stories
/config set news_digest.max_deep_fetch_total 30  # Deep-fetch 30 articles
```

---

### 12.2 Deep Search

**Tool name:** `deep_search`
**Timeout:** 600 seconds (10 minutes)

Multi-round web research tool. Performs iterative searches across multiple angles, parallel page crawling, reference chasing, and generates structured reports.

#### Parameters

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `query` | string | *(required)* | Research topic or question |
| `depth` | 1-3 | 2 | Research depth level |
| `max_results` | 1-10 | 8 | Results per search round |
| `search_engine` | string | auto | `perplexity`, `duckduckgo`, `brave`, `you` |

**Depth levels:**
- **1 (Quick):** Single search round + crawl, ~1 minute, up to 10 pages
- **2 (Standard):** 3 search rounds + reference chasing, ~3 minutes, up to 30 pages
- **3 (Thorough):** 5 search rounds + aggressive link chasing, ~5 minutes, up to 50 pages

#### Output Files

Results are saved to `./research/<query-slug>/`:
- `_report.md` — Structured research report
- `_search_results.md` — Combined raw search results
- `01_<domain>.md`, `02_<domain>.md`, etc. — Full page content

#### Required Environment Variables

At least one: `PERPLEXITY_API_KEY` (preferred), `BRAVE_API_KEY`, or `YDC_API_KEY`. Falls back to DuckDuckGo if none is set.

#### Sample Chat Usage

```
User: Research the current state of AI regulations worldwide

Bot: [uses deep_search with query="AI regulations worldwide 2026", depth=2]
     🔍 Deep Research Report: AI Regulations Worldwide

     ## Summary
     AI regulation varies significantly by region...

     ## Key Findings
     1. **EU AI Act** — Entered into force in 2024...
     2. **US Executive Order** — ...
     3. **China's approach** — ...

     ## Sources
     - [EU AI Act official text](...)
     - [Congressional Research Service](...)
     ...

     Full report saved to: ./research/ai-regulations-worldwide-2026/
```

```
User: Do a quick search on Rust async runtime comparison

Bot: [uses deep_search with query="Rust async runtime comparison tokio vs async-std",
      depth=1]
     Quick research findings:
     - **Tokio** is the most widely used (90%+ of ecosystem)...
```

---

### 12.3 Deep Crawl

**Tool name:** `deep_crawl`
**Requires:** Google Chrome/Chromium in PATH

Recursively crawls a website using headless Chrome via CDP (Chrome DevTools Protocol). Renders JavaScript, follows same-origin links via BFS, extracts clean text from each page.

#### Parameters

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `url` | string | *(required)* | Starting URL |
| `max_depth` | 1-10 | 3 | Maximum link-following depth |
| `max_pages` | 1-200 | 50 | Maximum pages to crawl |
| `path_prefix` | string | none | Only follow links with this path prefix |

#### Output

Saves crawled pages to `crawl-<hostname>/` directory:
- `000_index.md` — Landing page
- `001_docs_install.md` — First discovered page
- `002_...` — etc.

#### Sample Chat Usage

```
User: Crawl the Rust documentation at docs.rs/tokio, limit to the guide section

Bot: [uses deep_crawl with url="https://docs.rs/tokio/latest/tokio/",
      max_depth=3, max_pages=30, path_prefix="/tokio/"]
     Crawled 28 pages from docs.rs/tokio:

     Sitemap:
     - /tokio/ (index)
     - /tokio/runtime/ (runtime module)
     - /tokio/sync/ (synchronization primitives)
     ...

     Full content saved to: crawl-docs.rs/
```

```
User: Crawl my company's documentation site

Bot: [uses deep_crawl with url="https://docs.example.com", max_pages=100]
     Crawled 87 pages. Content saved to crawl-docs.example.com/
```

#### Configuration

```
/config set deep_crawl.page_settle_ms 5000      # Wait 5s for JS rendering
/config set deep_crawl.max_output_chars 100000   # Larger output limit
```

---

### 12.4 Send Email

**Tool name:** `send_email`

Sends emails via SMTP or Feishu/Lark Mail API.

#### Parameters

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `to` | string | *(required)* | Recipient email address |
| `subject` | string | *(required)* | Email subject |
| `body` | string | *(required)* | Email body (plain text or HTML) |
| `provider` | `"smtp"` / `"feishu"` | auto | Auto-detected from available env vars |
| `html` | boolean | false | Treat body as HTML |
| `attachments` | array | none | File attachments (SMTP only) |

#### SMTP Environment Variables

```bash
export SMTP_HOST="smtp.gmail.com"
export SMTP_PORT="465"
export SMTP_USERNAME="your-email@gmail.com"
export SMTP_PASSWORD="your-app-password"
export SMTP_FROM="your-email@gmail.com"
```

#### Feishu Mail Environment Variables

```bash
export LARK_APP_ID="cli_..."
export LARK_APP_SECRET="..."
export LARK_FROM_ADDRESS="your-feishu-email@company.com"
# Optional: LARK_REGION="global" for larksuite.com (default: feishu.cn)
```

#### Sample Chat Usage

```
User: Send an email to john@example.com with the subject "Meeting Notes"
      and include today's meeting summary

Bot: [uses send_email tool]
     Email sent to john@example.com with subject "Meeting Notes".
```

```
User: Send an HTML newsletter to newsletter@example.com

Bot: [uses send_email with html=true]
     Sent HTML email to newsletter@example.com.
```

```
User: Email the report.pdf to the team lead

Bot: [uses send_email with attachments=[{path: "/path/to/report.pdf"}]]
     Sent email with attachment report.pdf to team-lead@example.com.
```

---

### 12.5 Account Manager

**Tool name:** `manage_account`

Manages sub-accounts under the current profile. See [Section 9](#9-sub-account-management) for detailed usage and examples.

#### Actions

| Action | Description |
|--------|-------------|
| `list` | List all sub-accounts |
| `create` | Create a new sub-account |
| `update` | Update sub-account settings |
| `delete` | Delete a sub-account |
| `info` | Get detailed sub-account info |
| `start` | Start the sub-account's gateway |
| `stop` | Stop the sub-account's gateway |
| `restart` | Restart the sub-account's gateway |

#### Sample Chat Usage

```
User: Create a sub-account for my work team with a Telegram bot

Bot: [uses manage_account with action="create", name="work team",
      system_prompt="You are a work assistant for the engineering team.",
      telegram_token="123:ABC...", enable=true]
     Created sub-account "work team" (mybot--work-team) and started it.
     Telegram bot is now active.
```

---

### 12.6 Clock

**Tool name:** `get_time`
**Timeout:** 5 seconds
**Requires network:** No
**Context-triggered:** Activated when conversation mentions "time", "clock", "what time", "几点", "现在时间"

Returns current date, time, day of week, and UTC offset for any timezone.

#### Parameters

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `timezone` | string | server local | IANA timezone name |

**Common timezones:** `UTC`, `US/Eastern`, `US/Central`, `US/Pacific`, `Europe/London`, `Europe/Paris`, `Europe/Stockholm`, `Europe/Berlin`, `Asia/Shanghai`, `Asia/Tokyo`, `Asia/Seoul`, `Asia/Singapore`, `Australia/Sydney`

#### Sample Chat Usage

```
User: What time is it in Tokyo?

Bot: [uses get_time with timezone="Asia/Tokyo"]
     It's currently 2:30 PM on Thursday, March 6, 2026 in Tokyo (JST, UTC+9).
```

```
User: 现在纽约几点？

Bot: [uses get_time with timezone="US/Eastern"]
     纽约现在是凌晨12:30，2026年3月6日，星期五 (EST, UTC-5)。
```

---

### 12.7 Weather

**Tools:** `get_weather`, `get_forecast`
**Timeout:** 15 seconds
**API:** Open-Meteo (free, no API key required)
**Context-triggered:** Activated when conversation mentions "weather", "forecast", "temperature", "天气", "气温"

#### get_weather Parameters

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `city` | string | *(required)* | City name in English, optionally with country |

#### get_forecast Parameters

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `city` | string | *(required)* | City name in English |
| `days` | 1-16 | 7 | Forecast days |

**Note:** Always use English city names. Non-English names should be translated (e.g., "北京" → "Beijing").

#### Sample Chat Usage

```
User: What's the weather like in Paris?

Bot: [uses get_weather with city="Paris"]
     Current weather in Paris:
     🌤 Partly cloudy, 12°C
     💧 Humidity: 65%
     💨 Wind: 15 km/h NW
```

```
User: 上海未来一周天气怎么样？

Bot: [uses get_forecast with city="Shanghai", days=7]
     上海未来7天天气预报：

     周四 3/6: ☁ 8°C / 14°C — 多云
     周五 3/7: 🌧 6°C / 11°C — 小雨
     周六 3/8: ☀ 7°C / 16°C — 晴
     ...
```

```
User: Will it rain in New York this weekend?

Bot: [uses get_forecast with city="New York, US", days=5]
     Looking at the New York forecast:
     - Saturday: 30% chance of rain, 8°C/15°C
     - Sunday: Clear skies, 10°C/18°C
     Looks like Saturday might have some light rain, but Sunday should be clear!
```

### 12.8 WeChat Bridge

**Binary:** `wechat-bridge`

WebSocket bridge for WeChat personal accounts. Connects to the WeChat client via WebSocket and forwards messages to the gateway.

### 12.9 Pipeline Guard

**Type:** Hook (not a tool)
**Event:** `before_tool_call` (filter: `run_pipeline`)

Validates DOT graphs and injects optimal model assignments before `run_pipeline` executes. Runs as a before-hook with 10s timeout — can deny malformed pipeline submissions.

---

## 13. Platform Skills (ASR/TTS)

Platform skills are server-level skills that require the OminiX backend running on Apple Silicon. They provide on-device voice transcription and synthesis — no cloud APIs needed.

### 13.1 Prerequisites

- Apple Silicon Mac (M1/M2/M3/M4)
- OminiX API server running (managed via `octos serve`)
- Models downloaded: `Qwen3-ASR-1.7B-8bit`, `Qwen3-TTS-12Hz-1.7B-CustomVoice-8bit`

### 13.2 Managing OminiX via Dashboard

The dashboard provides controls for:
- Starting/stopping the OminiX engine
- Viewing logs
- Downloading/removing models
- Checking service health

Or via admin API:

```bash
# Start OminiX
curl -X POST http://localhost:3000/api/admin/platform-skills/ominix-api/start

# Check health
curl http://localhost:3000/api/admin/platform-skills/asr/health

# Download a model
curl -X POST http://localhost:3000/api/admin/platform-skills/ominix-api/models/download \
  -H "Content-Type: application/json" \
  -d '{"model_id": "Qwen3-ASR-1.7B-8bit"}'

# View logs
curl http://localhost:3000/api/admin/platform-skills/ominix-api/logs?lines=100
```

### 13.3 Voice Transcription (`voice_transcribe`)

Transcribes audio files to text.

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `audio_path` | string | *(required)* | Absolute path to audio file (WAV, OGG, MP3, FLAC, M4A) |
| `language` | string | `"Chinese"` | `"Chinese"`, `"English"`, `"Japanese"`, `"Korean"`, `"Cantonese"` |

```
User: Transcribe this audio file /tmp/meeting.wav

Bot: [uses voice_transcribe with audio_path="/tmp/meeting.wav", language="Chinese"]
     Transcription:
     "大家好，今天的会议主要讨论三个议题..."
```

### 13.4 Voice Synthesis (`voice_synthesize`)

Converts text to speech using preset voices.

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `text` | string | *(required)* | Text to synthesize |
| `output_path` | string | `/tmp/octos_tts_<ts>.wav` | Output file path |
| `language` | string | `"chinese"` | `"chinese"`, `"english"`, `"japanese"`, `"korean"` |
| `speaker` | string | `"vivian"` | Voice preset |

**Available voices:**
- **English/Chinese:** `vivian`, `serena`, `ryan`, `aiden`, `eric`, `dylan`
- **Chinese only:** `uncle_fu`
- **Japanese:** `ono_anna`
- **Korean:** `sohee`

```
User: Read this text aloud: "Welcome to the daily briefing"

Bot: [uses voice_synthesize with text="Welcome to the daily briefing",
      language="english", speaker="ryan"]
     [sends audio file to user]
```

### 13.5 Voice Cloning (`voice_clone_synthesize`)

Synthesizes speech in a cloned voice from a reference audio sample.

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `text` | string | *(required)* | Text to synthesize |
| `reference_audio` | string | *(required)* | Path to reference audio (3-10 seconds) |
| `output_path` | string | auto | Output file path |
| `language` | string | `"chinese"` | Target language |

```
User: Clone my voice from this sample and say "Good morning team"
      Reference: /tmp/my-voice-sample.wav

Bot: [uses voice_clone_synthesize with reference_audio="/tmp/my-voice-sample.wav",
      text="Good morning team", language="english"]
     Generated speech in your voice. [sends audio]
```

### 13.6 Podcast Generation (`generate_podcast`)

Creates multi-speaker podcast audio from a script.

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `script` | array | *(required)* | Array of `{speaker, voice, text}` objects |
| `output_path` | string | auto | Output file path |
| `language` | string | `"chinese"` | Language |

```
User: Generate a short podcast episode about AI safety with two speakers

Bot: [uses generate_podcast with script=[
      {speaker: "Host", voice: "vivian", text: "Welcome to AI Weekly..."},
      {speaker: "Guest", voice: "ryan", text: "Thanks for having me..."},
      ...
    ], language="english"]
     Generated podcast (2:30 duration). [sends audio file]
```

### 13.7 Gateway Voice Configuration

Auto-transcription and auto-TTS for voice messages from messaging channels:

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

- **`auto_asr`**: Automatically transcribe incoming voice/audio messages before sending to the agent
- **`auto_tts`**: Automatically synthesize voice replies when the user sends voice
- **`default_voice`**: Voice preset for auto-TTS
- **`asr_language`**: Force a specific language for transcription (`null` = auto-detect)

---

## 14. Custom Skill Installation

Custom skills extend the agent with new tools and instructions. They can be installed from GitHub repositories or created locally.

### 14.1 Installing from GitHub

```bash
# Install all skills from a repo
octos skills install user/repo

# Install a specific skill subdirectory
octos skills install user/repo/skill-name

# Install from a specific branch
octos skills install user/repo --branch develop

# Force overwrite existing
octos skills install user/repo --force

# Install into a specific profile
octos skills install user/repo --profile my-bot
```

**Installation process:**
1. Tries to download pre-built binary from the skill registry (SHA-256 verified)
2. Falls back to `cargo build --release` if `Cargo.toml` is present
3. Runs `npm install` if `package.json` is present
4. Writes `.source` file for update tracking

### 14.2 Managing Skills

```bash
# List installed skills
octos skills list

# Show detailed skill info
octos skills info skill-name

# Update a specific skill
octos skills update skill-name

# Update all skills
octos skills update all

# Remove a skill
octos skills remove skill-name

# Search the online registry
octos skills search "web scraping"
```

### 14.3 Skill Directory Structure

A skill lives in `.octos/skills/<name>/` and contains:

```
.octos/skills/my-skill/
├── SKILL.md         # Required: instructions + frontmatter
├── manifest.json    # Required for tool skills: tool definitions
├── main             # Compiled binary (or script)
└── .source          # Auto-generated: tracks install source
```

### 14.4 SKILL.md Format

```markdown
---
name: my-skill
version: 1.0.0
author: Your Name
description: A brief description of what this skill does
always: false
requires_bins: curl,jq
requires_env: MY_API_KEY
---

# My Skill Instructions

Instructions for the agent on how and when to use this skill.

## When to Use
- Use this skill when the user asks about...

## Tool Usage
The `my_tool` tool accepts:
- `query` (required): The search query
- `limit` (optional): Maximum results (default: 10)

## Examples
User: "Find me information about X"
→ Use my_tool with query="X"
```

**Frontmatter fields:**
- **`name`** — Skill identifier (must match directory name)
- **`version`** — Semantic version
- **`author`** — Skill author
- **`description`** — Short description
- **`always`** — If `true`, skill instructions are always included in the system prompt. If `false`, the agent can read them on demand.
- **`requires_bins`** — Comma-separated binary names checked via `which`. Skill is unavailable if any are missing.
- **`requires_env`** — Comma-separated environment variable names. Skill is unavailable if any are unset.

### 14.5 manifest.json Format

For skills that provide executable tools:

```json
{
  "name": "my-skill",
  "version": "1.0.0",
  "description": "My custom skill",
  "tools": [
    {
      "name": "my_tool",
      "description": "Does something useful",
      "timeout_secs": 60,
      "input_schema": {
        "type": "object",
        "properties": {
          "query": {
            "type": "string",
            "description": "The search query"
          },
          "limit": {
            "type": "integer",
            "description": "Maximum results",
            "default": 10
          }
        },
        "required": ["query"]
      }
    }
  ],
  "entrypoint": "main"
}
```

The tool binary receives JSON input on stdin and outputs JSON on stdout:

```json
// Input (stdin)
{"query": "test", "limit": 5}

// Output (stdout)
{"output": "Results here...", "success": true}
```

### 14.6 Skill Resolution Order

Skills are loaded from these directories (in priority order):

1. `.octos/plugins/` (legacy)
2. `.octos/skills/` (user-installed custom skills)
3. `.octos/bundled-app-skills/` (bundled: news, deep-search, etc.)
4. `.octos/platform-skills/` (platform: asr/tts)
5. `~/.octos/plugins/` (global legacy)
6. `~/.octos/skills/` (global custom)

User-installed skills override bundled skills with the same name.

### 14.7 Creating a Custom Skill

#### Example: A Translation Skill (Python)

1. Create the skill directory:

```bash
mkdir -p .octos/skills/translator
```

2. Create `SKILL.md`:

```markdown
---
name: translator
version: 1.0.0
description: Translate text between languages using DeepL API
always: false
requires_env: DEEPL_API_KEY
---

# Translator Skill

Use the `translate` tool when the user asks to translate text between languages.

## Usage
- `text` (required): Text to translate
- `target_lang` (required): Target language code (EN, DE, FR, JA, ZH, etc.)
- `source_lang` (optional): Source language code (auto-detected if omitted)
```

3. Create `manifest.json`:

```json
{
  "name": "translator",
  "version": "1.0.0",
  "tools": [
    {
      "name": "translate",
      "description": "Translate text between languages using DeepL",
      "timeout_secs": 30,
      "input_schema": {
        "type": "object",
        "properties": {
          "text": {"type": "string", "description": "Text to translate"},
          "target_lang": {"type": "string", "description": "Target language code"},
          "source_lang": {"type": "string", "description": "Source language code"}
        },
        "required": ["text", "target_lang"]
      }
    }
  ],
  "entrypoint": "main"
}
```

4. Create `main` (executable script):

```python
#!/usr/bin/env python3
import json, sys, os, urllib.request

input_data = json.loads(sys.stdin.read())
text = input_data["text"]
target = input_data["target_lang"]
source = input_data.get("source_lang", "")

api_key = os.environ["DEEPL_API_KEY"]
data = json.dumps({
    "text": [text],
    "target_lang": target,
    **({"source_lang": source} if source else {})
}).encode()

req = urllib.request.Request(
    "https://api-free.deepl.com/v2/translate",
    data=data,
    headers={"Authorization": f"DeepL-Auth-Key {api_key}", "Content-Type": "application/json"}
)

with urllib.request.urlopen(req) as resp:
    result = json.loads(resp.read())
    translated = result["translations"][0]["text"]
    print(json.dumps({"output": translated, "success": True}))
```

5. Make it executable:

```bash
chmod +x .octos/skills/translator/main
```

6. Test it:

```
User: Translate "Hello world" to Japanese

Bot: [uses translate tool with text="Hello world", target_lang="JA"]
     Translation: こんにちは世界
```

---

## 15. Configuration Reference

### 15.1 Full Config Structure

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

  // Deployment role for octos serve
  "mode": "local",           // local | tenant | cloud
  "tunnel_domain": null,      // optional for tenant/cloud tunnel setups
  "frps_server": null,        // optional for tenant/cloud tunnel setups

  // Dashboard auth (serve mode only)
  "dashboard_auth": null,

  // Monitor (serve mode only)
  "monitor": null
}
```

### 15.2 Environment Variables

| Variable | Description |
|----------|-------------|
| **LLM Providers** | |
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
| **Search** | |
| `BRAVE_API_KEY` | Brave Search API key |
| `PERPLEXITY_API_KEY` | Perplexity Sonar API key |
| `YDC_API_KEY` | You.com API key |
| **Channels** | |
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
| **Email (send-email skill)** | |
| `SMTP_HOST` | SMTP server hostname |
| `SMTP_PORT` | SMTP server port |
| `SMTP_USERNAME` | SMTP username |
| `SMTP_PASSWORD` | SMTP password |
| `SMTP_FROM` | SMTP from address |
| `LARK_APP_ID` | Feishu mail app ID |
| `LARK_APP_SECRET` | Feishu mail app secret |
| `LARK_FROM_ADDRESS` | Feishu mail from address |
| **Voice** | |
| `OMINIX_API_URL` | OminiX ASR/TTS API URL |
| **System** | |
| `RUST_LOG` | Log level (error/warn/info/debug/trace) |
| `OCTOS_LOG_JSON` | Enable JSON-formatted logs (set to any value) |
| `OCTOS_HOME` | Override the global data/config directory (default: `~/.octos`) |
| `TUNNEL_DOMAIN` | Tunnel base domain for tenant/cloud deployments |
| `FRPS_SERVER` | frps relay host for tenant/cloud deployments |

### 15.3 File Layout

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
├── TOOLS.md                    # Tool-specific guidance
├── IDENTITY.md                 # Custom identity
├── HEARTBEAT.md                # Background task instructions
├── sessions/                   # Conversation history (JSONL)
├── memory/                     # Memory files
│   ├── MEMORY.md               # Long-term persistent memory
│   └── 2026-03-06.md           # Daily notes
├── skills/                     # Custom skills
│   ├── news/                   # Bundled: news fetch
│   ├── deep-search/            # Bundled: deep web search
│   ├── deep-crawl/             # Bundled: deep crawl
│   ├── send-email/             # Bundled: email sending
│   ├── account-manager/        # Bundled: sub-account management
│   ├── clock/                  # Bundled: time queries
│   ├── weather/                # Bundled: weather info
│   └── my-custom-skill/        # User-installed skill
├── platform-skills/            # Platform skills (ASR/TTS)
├── episodes.redb               # Episodic memory database
├── tool_config.json            # Tool configuration overrides
└── history/
    └── chat_history            # Readline history (CLI)
```

---

## 16. Matrix Appservice (Palpo)

Octos can run as a [Matrix Application Service](https://spec.matrix.org/latest/application-service-api/) (appservice) behind a Matrix homeserver. This section describes how to deploy Octos alongside [Palpo](https://github.com/palpo-im/palpo) using Docker Compose so that users can talk to the bot from any Matrix client.

### 16.1 How It Works

```
Matrix Client (Element, etc.)
       │
       ▼
  Palpo (homeserver :8008)
       │  pushes events via Appservice API
       ▼
  Octos (appservice listener :8009)
       │  sends responses back via Palpo's client-server API
       ▼
  Palpo ──► Matrix Client
```

Palpo loads a **registration YAML** at startup that tells it which user namespaces belong to Octos and where to forward events. Octos listens on a dedicated port (default `8009`) for those events and replies through Palpo's client-server API.

### 16.2 Directory Layout

```
palpo_with_octos/
├── compose.yml                        # Docker Compose file
├── palpo.toml                         # Palpo homeserver config
├── appservices/
│   └── octos-registration.yaml        # Appservice registration
├── config/
│   ├── botfather.json                 # Octos profile (Matrix channel)
│   └── octos.json                     # Octos global config
├── data/
│   ├── pgsql/                         # PostgreSQL data
│   ├── octos/                         # Octos runtime data
│   └── media/                         # Palpo media store
└── static/
    └── index.html                     # Palpo home page
```

### 16.3 Step-by-Step Setup

#### 1. Generate Tokens

The appservice registration and the Octos profile must share two tokens. Generate them once:

```bash
# Generate as_token and hs_token (any random hex string works)
openssl rand -hex 32   # → as_token
openssl rand -hex 32   # → hs_token
```

Keep them handy — you will paste them into two files below.

#### 2. Create the Appservice Registration

Create `appservices/octos-registration.yaml`:

```yaml
# Matrix Appservice Registration — octos
id: octos-matrix-appservice

# URL where Palpo pushes events to octos (Docker service name, NOT localhost)
url: "http://octos:8009"

# Tokens — must match config/botfather.json
as_token: "<your-as-token>"
hs_token: "<your-hs-token>"

sender_localpart: octosbot
rate_limited: false

namespaces:
  users:
    - exclusive: true
      regex: "@octosbot_.*:your\\.server\\.name"
    - exclusive: true
      regex: "@octosbot:your\\.server\\.name"
  aliases: []
  rooms: []
```

Key fields:

| Field | Description |
|-------|-------------|
| `url` | Where Palpo sends events. Use the Docker service name (e.g. `http://octos:8009`), not `localhost`. |
| `as_token` | Token that Octos uses when calling Palpo's API. |
| `hs_token` | Token that Palpo uses when pushing events to Octos. |
| `sender_localpart` | The bot's Matrix local username (becomes `@octosbot:your.server.name`). |
| `namespaces.users` | Regex patterns for user IDs the appservice manages. Include both the bot itself and any bridged-user prefix. |

#### 3. Configure Palpo

In `palpo.toml`, point to the directory containing the registration file:

```toml
server_name = "your.server.name"
listen_addr = "0.0.0.0:8008"

allow_registration = true
allow_federation = true

# Palpo auto-loads all .yaml files from this directory on startup
appservice_registration_dir = "/var/palpo/appservices"

[db]
url = "postgres://palpo:<db-password>@palpo_postgres:5432/palpo"
pool_size = 10

[well_known]
server = "your.server.name"
client = "https://your.server.name"
```

#### 4. Create the Octos Profile

Create `config/botfather.json` with a Matrix channel that uses the same tokens:

```json
{
  "id": "botfather",
  "name": "BotFather",
  "enabled": true,
  "config": {
    "provider": "deepseek",
    "model": "deepseek-chat",
    "api_key_env": "DEEPSEEK_API_KEY",
    "channels": [
      {
        "type": "matrix",
        "homeserver": "http://palpo:8008",
        "as_token": "<your-as-token>",
        "hs_token": "<your-hs-token>",
        "server_name": "your.server.name",
        "sender_localpart": "octosbot",
        "user_prefix": "octosbot_",
        "port": 8009,
        "allowed_senders": ["@alice:your.server.name"]
      }
    ],
    "gateway": {
      "max_history": 50,
      "queue_mode": "followup"
    }
  }
}
```

Matrix channel fields:

| Field | Description |
|-------|-------------|
| `type` | Must be `"matrix"`. |
| `homeserver` | Palpo's internal URL (Docker service name). |
| `as_token` / `hs_token` | Must match the registration YAML. |
| `server_name` | The Matrix domain (must match `palpo.toml`). |
| `sender_localpart` | Bot username (must match the registration). |
| `user_prefix` | Prefix for bridged user IDs managed by this appservice. |
| `port` | Port Octos listens on for appservice events from Palpo. |
| `allowed_senders` | Matrix user IDs that may talk to the bot. Empty array = allow all. |

#### 5. Docker Compose

```yaml
services:
  palpo_postgres:
    image: postgres:17
    restart: always
    volumes:
      - ./data/pgsql:/var/lib/postgresql/data
    environment:
      POSTGRES_PASSWORD: <db-password>
      POSTGRES_USER: palpo
      POSTGRES_DB: palpo
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U palpo"]
      interval: 5s
      timeout: 5s
      retries: 5
    networks:
      - internal

  palpo:
    image: ghcr.io/palpo-im/palpo:latest
    restart: unless-stopped
    ports:
      - 8128:8008     # Client-server API
      - 8348:8448     # Federation API
    environment:
      PALPO_CONFIG: "/var/palpo/palpo.toml"
    volumes:
      - ./palpo.toml:/var/palpo/palpo.toml:ro
      - ./appservices:/var/palpo/appservices:ro
      - ./data/media:/var/palpo/media
      - ./static:/var/palpo/static:ro
    depends_on:
      palpo_postgres:
        condition: service_healthy
    networks:
      - internal

  octos:
    build:
      context: /path/to/octos       # Path to Octos source repo
      dockerfile: Dockerfile
    restart: unless-stopped
    ports:
      - 8009:8009     # Appservice listener (receives events from Palpo)
      - 8010:8080     # Octos dashboard / admin API
    environment:
      DEEPSEEK_API_KEY: ${DEEPSEEK_API_KEY}
      RUST_LOG: octos=debug,info
    volumes:
      - ./data/octos:/root/.octos
      - ./config/botfather.json:/root/.octos/profiles/botfather.json:ro
      - ./config/octos.json:/config/octos.json:ro
    command: ["serve", "--host", "0.0.0.0", "--port", "8080", "--config", "/config/octos.json"]
    depends_on:
      - palpo
    networks:
      - internal

networks:
  internal:
    attachable: true
```

#### 6. Start Everything

```bash
docker compose up -d
```

Palpo reads `appservices/octos-registration.yaml` on startup. When a Matrix user sends a message in a room where the bot is invited, Palpo pushes the event to `http://octos:8009`, Octos processes it through the agent loop, and replies via Palpo's client-server API.

### 16.4 Token Matching Checklist

The most common misconfiguration is a token mismatch. All three of these must agree:

| Value | `octos-registration.yaml` | `botfather.json` |
|-------|--------------------------|-------------------|
| `as_token` | `as_token: "abc..."` | `"as_token": "abc..."` |
| `hs_token` | `hs_token: "def..."` | `"hs_token": "def..."` |
| `sender_localpart` | `sender_localpart: octosbot` | `"sender_localpart": "octosbot"` |
| server name | `regex: "@octosbot:your\\.server\\.name"` | `"server_name": "your.server.name"` |

### 16.5 Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| Bot does not respond | Token mismatch between registration and profile | Verify the [token checklist](#164-token-matching-checklist) |
| `Connection refused` in Palpo logs | Octos not running or wrong `url` in registration | Ensure Octos is up; use Docker service name (`http://octos:8009`), not `localhost` |
| `User ID not in namespace` | `sender_localpart` doesn't match registration `namespaces.users` regex | Update the regex to include the bot's full user ID |
| Messages from unauthorized users ignored | `allowed_senders` filtering | Add the user's Matrix ID to the array, or set it to `[]` to allow everyone |

---

*This guide covers Octos version as of March 2026. For the latest updates, see the repository at [github.com/octos-org/octos](https://github.com/octos-org/octos).*
