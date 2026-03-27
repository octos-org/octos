# Octos 🐙

**Open Cognitive Tasks Orchestration System**

> Like an octopus — 9 brains, every arm thinks independently, but they share one brain.

Octos is an open-source AI agent platform built in Rust. It turns any LLM into a multi-channel, multi-user intelligent assistant — deployed as a single 31MB binary with zero runtime dependencies.

Connect your LLM API keys and messaging channels. Octos handles conversation routing, tool execution, memory, provider failover, and multi-tenant isolation. Manage hundreds of AI agent profiles through a web dashboard or 91 REST endpoints.

## Repositories

| Repo | Description |
|------|-------------|
| **[octos](https://github.com/octos-org/octos)** | Core platform — Rust binary, 14 LLM providers, 14 channels, DOT pipeline engine, multi-tenant gateway, web dashboard |
| **[octos-hub](https://github.com/octos-org/octos-hub)** | Community skill registry — install and share agent skills |
| **[octos-web](https://github.com/octos-org/octos-web)** | Admin dashboard — React SPA for profile management, metrics, and fleet control |

## Key Capabilities

- **API-first** — 91 REST endpoints. Build any frontend on top: web, mobile, CLI, CI/CD
- **Multi-tenant** — 200+ profiles on 16GB. Each profile is an isolated OS process. Family Plan sub-accounts
- **Multi-LLM pipelines** — DOT graph workflows. Per-node model selection. Dynamic parallel fan-out
- **Adaptive routing** — 3-layer failover with Hedge racing, Lane scoring, and circuit breakers
- **LRU tool deferral** — 15 active tools for fast LLM reasoning, 34+ available on demand
- **14 channels** — Telegram, Discord, Slack, WhatsApp, Feishu, Email, WeCom, WeChat, Matrix, QQ Bot, Twilio, API, CLI
- **3-layer memory** — Long-term (entity bank), episodic (task outcomes in redb), session (JSONL + LLM compaction)
- **5 queue modes** — Followup, Collect, Steer, Interrupt, Speculative — per-session agent concurrency control
- **Native office suite** — PPTX/DOCX/XLSX via pure Rust (zip + quick-xml)
- **Sandbox isolation** — bwrap + sandbox-exec + Docker. `deny(unsafe_code)` workspace-wide

## Quick Start

```bash
cargo install --path crates/octos-cli
export ANTHROPIC_API_KEY=your-key
octos chat
```

## Links

- [User Guide (English)](https://github.com/octos-org/octos/blob/main/docs/user-guide.md)
- [用户指南 (中文)](https://github.com/octos-org/octos/blob/main/docs/user-guide-zh.md)
- [中文 README](https://github.com/octos-org/octos/blob/main/README-zh.md)

---

*Built with Rust. Powered by 9 brains.*
