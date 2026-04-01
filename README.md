# Octos 🐙

> Like an octopus — 9 brains (1 central + 8 in each arm), every arm thinks independently, but they share one brain.

**Open Cognitive Tasks Orchestration System** — a Rust-native, API-first Agentic OS.

31MB static binary. 91 REST endpoints. 14 LLM providers. 14 messaging channels. Multi-tenant. Zero dependencies.

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
- **LRU tool deferral**: 15 active tools for fast LLM reasoning, 34+ on demand. Idle tools auto-evict.
- **5 queue modes per session**: Followup, Collect, Steer, Interrupt, Speculative — users control agent concurrency via `/queue`.
- **Session control in any channel**: `/new`, `/s <name>`, `/sessions`, `/back` — works in Telegram, Discord, Slack, WhatsApp.
- **3-layer memory**: Long-term (entity bank, auto-injected), episodic (task outcomes in redb), session (JSONL + LLM compaction).
- **Native office suite**: PPTX/DOCX/XLSX via pure Rust (zip + quick-xml).
- **Sandbox isolation**: bwrap + sandbox-exec + Docker. `deny(unsafe_code)` workspace-wide. 67 prompt injection tests.

## Install (no dependencies required)

For standalone machines (Mac Mini, Linux server, etc.) — no Rust, Xcode, or development tools needed:

```bash
curl -fsSL https://github.com/octos-org/octos/releases/latest/download/install.sh | bash
```

Supported platforms: **macOS ARM64** (Apple Silicon), **Linux x86_64**, and **Linux ARM64**. Installs to `~/.octos/bin`, initializes the workspace, and sets up `octos serve` as a system service (launchd on macOS, systemd on Linux).

On **Windows** (PowerShell):

```powershell
irm https://github.com/octos-org/octos/releases/latest/download/install.ps1 | iex
```

With tunnel options (remote access via frpc, macOS/Linux only):

```bash
curl -fsSL https://github.com/octos-org/octos/releases/latest/download/install.sh | bash -s -- \
  --tenant-name alice --frps-token <token>
```

Diagnose an existing installation:

```bash
# macOS / Linux
curl -fsSL https://github.com/octos-org/octos/releases/latest/download/octos-doctor.sh | bash

# Windows
.\install.ps1 -Doctor
```

## Quick Start

```bash
# Build and install
cargo install --path crates/octos-cli

# Initialize workspace
octos init

# Set API key
export ANTHROPIC_API_KEY=your-key-here

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

**中文:** [中文 README](README-zh.md) | [用户指南](https://octos-org.github.io/octos/) (doc site)

## Architecture

```
octos serve (control plane + dashboard)
  ├── Profile A → gateway process (Telegram, WhatsApp)
  ├── Profile B → gateway process (Feishu, Slack)
  └── Profile C → gateway process (CLI)
       │
       ├── LLM Provider (Anthropic, OpenAI, Gemini, DeepSeek, ...)
       │   └── AdaptiveRouter → ProviderChain → RetryProvider
       ├── Tool Registry (13 built-in + 12 agent + 8 app-skills)
       │   └── LRU Deferral (15 active, activate on demand)
       ├── Pipeline Engine (DOT graphs, per-node model, parallel fan-out)
       ├── Session Store (JSONL, LRU cache, LLM compaction)
       ├── Memory (MEMORY.md + entity bank + episodes.redb + HNSW)
       └── Skills (bundled + installable from octos-hub)
```

## License

See [LICENSE](LICENSE).
