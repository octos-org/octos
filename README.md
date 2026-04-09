# Octos 🐙

> Like an octopus — 9 brains (1 central + 8 in each arm), every arm thinks independently, but they share one brain.

**Open Cognitive Tasks Orchestration System** — a Rust-native, API-first Agentic OS.

31MB static binary. 91 REST endpoints. 15 LLM providers. 14 messaging channels. Multi-tenant. Zero dependencies.

## What is Octos?

Octos is an open-source AI agent platform that turns any LLM into a multi-channel, multi-user intelligent assistant. You deploy a single Rust binary, connect your LLM API keys and messaging channels (Telegram, Discord, Slack, WhatsApp, Email, WeChat, and more), and Octos handles everything else — conversation routing, tool execution, memory, provider failover, and multi-tenant isolation.

Think of it as the **backend operating system for AI agents**. Instead of building a chatbot from scratch for each use case, you configure Octos profiles — each with their own system prompt, model, tools, and channels — and manage them all through a web dashboard or REST API. A small team can run hundreds of specialized AI agents on a single machine.

Octos is built for people who need more than a personal assistant: teams deploying AI for customer support across WhatsApp and Telegram, developers building AI-powered products on top of a REST API, researchers orchestrating multi-step research pipelines with different LLMs at each stage, or families sharing a single AI setup with per-person customization.

## Why Octos

Most agentic systems are single-tenant chat assistants — one user, one model, one conversation at a time. Octos is different:

- **API-first Agentic OS**: 91 REST endpoints (chat, sessions, admin, profiles, skills, metrics, webhooks). Any frontend — web, mobile, CLI, CI/CD — can be built on top.
- **Multi-tenant by design**: One 31MB binary serves 200+ profiles on a 16GB machine. Each profile is a separate OS process with isolated memory, sessions, and data. Family Plan sub-accounts.
- **Multi-LLM DOT pipelines**: Define workflows as DOT graphs. Per-node model selection. Dynamic parallel fan-out spawns N concurrent workers at runtime.
- **3-layer provider failover**: RetryProvider → ProviderChain → AdaptiveRouter. Hedge racing, Lane scoring, circuit breakers.
- **LRU tool deferral**: 15 active tools for fast LLM reasoning, 34+ on demand. Idle tools auto-evict. `spawn_only` tools auto-redirect to background execution.
- **5 queue modes per session**: Followup, Collect, Steer, Interrupt, Speculative — users control agent concurrency via `/queue`.
- **Session control in any channel**: `/new`, `/s <name>`, `/sessions`, `/back` — works in Telegram, Discord, Slack, WhatsApp.
- **3-layer memory**: Long-term (entity bank, auto-injected), episodic (task outcomes in redb), session (JSONL + LLM compaction).
- **Native office suite**: PPTX/DOCX/XLSX via pure Rust (zip + quick-xml).
- **Sandbox isolation**: bwrap + sandbox-exec + Docker. `deny(unsafe_code)` workspace-wide. 67 prompt injection tests.

## Install (no dependencies required)

One command installs octos on your Mac Mini, Linux server, or Windows PC — no Rust, Xcode, or development tools needed:

```bash
# macOS / Linux
curl -fsSL https://github.com/octos-org/octos/releases/latest/download/install.sh | bash
```

```powershell
# Windows (PowerShell)
irm https://github.com/octos-org/octos/releases/latest/download/install.ps1 | iex
```

This installs the binary, sets up `octos serve` as a system service, and starts the local dashboard at `http://localhost:8080/admin/`.

Supported platforms: **macOS ARM64** (Apple Silicon), **Linux x86_64**, **Linux ARM64**, and **Windows x64**.

### After install

The install script saves itself locally, so you can re-run without downloading again:

```bash
# macOS / Linux
~/.octos/bin/install.sh --tunnel    # Enable public tunnel
~/.octos/bin/install.sh --doctor    # Diagnose issues
```

```powershell
# Windows
& "$HOME\.octos\bin\install.ps1" -Tunnel    # Enable public tunnel
& "$HOME\.octos\bin\install.ps1" -Doctor    # Diagnose issues
```

### Cloud signup

Register at [octos-cloud.org](https://octos-cloud.org) to get a personalized install command with your machine name, auth token, and SSH port pre-filled — for both macOS/Linux and Windows. The registration flow enables the tunnel by default and prompts you to supply the host's shared FRPS token. If SMTP is configured on the server, the setup details are also emailed as backup.

### Cloud host bootstrap

To bootstrap the relay/host server itself, run the new host bootstrap script on a Linux VPS:

```bash
bash scripts/cloud-host-deploy.sh
```

It wraps the three host-side steps in order:

- `scripts/install.sh` for `octos serve`
- `scripts/frp/setup-frps.sh` for the relay
- `scripts/frp/setup-caddy.sh` for apex and wildcard routing

For silent reruns, it supports `--config <env-file>` and persists the chosen settings to `./cloud-bootstrap.env` in the current working directory by default.

### Deployment modes

octos supports three deployment modes via `"mode"` in `~/.octos/config.json`:

- **`local`** (default) — Standalone machine. Dashboard at `/admin/`.
- **`tenant`** — End-user machine with optional tunnel to a cloud relay.
- **`cloud`** — VPS relay server with tenant management and public signup page.

`~/.octos/config.json` is the runtime config that `octos serve` loads on startup. Direct installers such as `scripts/install.sh` and `scripts/install.ps1` create it for local or tenant machines; `scripts/cloud-host-deploy.sh` now creates or updates it for host machines with `mode = "cloud"` plus `tunnel_domain` and `frps_server`.

### Optional features

```bash
# Auto-install runtime dependencies (git, node, python, ffmpeg, chromium)
curl ... | bash -s -- --install-deps

# Set up Caddy reverse proxy with HTTPS (for self-hosted deployments)
curl ... | bash -s -- --caddy-domain crew.example.com
```

## Quick Start

```bash
# Build and install
cargo install --path crates/octos-cli

# Initialize workspace
octos init

# Set API key (any supported provider — auto-detected during install)
export OPENAI_API_KEY=your-key-here    # or ANTHROPIC_API_KEY, GEMINI_API_KEY, etc.

# Interactive chat
octos chat

# Multi-channel gateway
octos gateway

# Web dashboard + REST API
octos serve
```

## Documentation

📖 **[Full Documentation](https://octos-org.github.io/octos/)** — installation, configuration, channels, providers, memory, skills, advanced features, and more.

**Quick links:**
- [Installation & Deployment](https://octos-org.github.io/octos/installation.html)
- [Configuration](https://octos-org.github.io/octos/configuration.html)
- [LLM Providers & Routing](https://octos-org.github.io/octos/providers.html)
- [Gateway & Channels](https://octos-org.github.io/octos/channels.html)
- [Memory & Skills](https://octos-org.github.io/octos/memory-skills.html)
- [Advanced Features](https://octos-org.github.io/octos/advanced.html) (queue modes, hooks, sandbox, tools)
- [CLI Reference](https://octos-org.github.io/octos/cli-reference.html)
- [Skill Development](https://octos-org.github.io/octos/skill-development.html)

**中文:** [中文 README](README-zh.md) | [用户指南](https://octos-org.github.io/octos/zh/) (doc site)

## Architecture

```
octos serve (control plane + dashboard)
  ├── Profile A → gateway process (Telegram, WhatsApp)
  ├── Profile B → gateway process (Feishu, Slack)
  └── Profile C → gateway process (CLI)
       │
       ├── LLM Provider (Anthropic, OpenAI, Gemini, DeepSeek, ...)
       │   └── AdaptiveRouter → ProviderChain → RetryProvider
       ├── Tool Registry (25 built-in + plugins + 9 app-skills)
       │   └── LRU Deferral (15 active, activate on demand)
       ├── Pipeline Engine (DOT graphs, per-node model, parallel fan-out)
       ├── Session Store (JSONL, LRU cache, LLM compaction)
       ├── Memory (MEMORY.md + entity bank + episodes.redb + HNSW)
       └── Skills (bundled + installable from octos-hub)
```

## License

See [LICENSE](LICENSE).
