# Product Requirements Document: octos

## Executive Summary

octos is a Rust-native AI agent framework that provides both a coding automation CLI and a multi-channel messaging gateway. It supports 14 LLM providers, 8 messaging channels (CLI + 7 platforms), and a rich tool system for autonomous task execution.

## Problem Statement

1. **Fragmented AI tools**: Existing solutions are Python-based, slow, or tied to specific providers
2. **No multi-channel support**: Most coding agents only work via CLI
3. **Vendor lock-in**: Switching providers requires code changes

## Target Users

- **Individual developers**: Automate coding tasks via CLI or chat
- **Teams**: Deploy AI assistant across Slack, Discord, Telegram, etc.
- **DevOps engineers**: Integrate AI into CI/CD with cron and scheduled tasks
- **International teams**: Chinese LLM support (DeepSeek, Qwen, Moonshot, Zhipu, MiniMax)

## Functional Requirements

### FR-1: Task Execution

| ID | Requirement | Status |
|----|-------------|--------|
| FR-1.1 | Interactive multi-turn chat mode | Done |
| FR-1.2 | Single-message mode (`--message` flag) | Done |
| FR-1.3 | Support iteration limits | Done |
| FR-1.4 | Real-time progress display | Done |
| FR-1.5 | Graceful interruption (Ctrl+C) | Done |

### FR-2: LLM Providers

| ID | Requirement | Status |
|----|-------------|--------|
| FR-2.1 | Anthropic Claude | Done |
| FR-2.2 | OpenAI GPT | Done |
| FR-2.3 | Google Gemini | Done |
| FR-2.4 | OpenRouter (aggregator) | Done |
| FR-2.5 | OpenAI-compatible providers (DeepSeek, Groq, Moonshot, DashScope, MiniMax, Zhipu) | Done |
| FR-2.6 | Local deployment (Ollama, vLLM) | Done |
| FR-2.7 | Provider auto-detect from model name | Done |
| FR-2.8 | Automatic retry with exponential backoff | Done |
| FR-2.9 | Custom base URL support | Done |
| FR-2.10 | Adaptive routing (metrics-driven provider selection) | Done |
| FR-2.11 | SwappableProvider for runtime model switching | Done |

### FR-3: Tool System

| ID | Requirement | Status |
|----|-------------|--------|
| FR-3.1 | Shell execution with SafePolicy | Done |
| FR-3.2 | File read/write/edit operations | Done |
| FR-3.3 | Glob and grep search | Done |
| FR-3.3b | Directory listing | Done |
| FR-3.4 | Web search and fetch | Done |
| FR-3.5 | Cross-channel messaging | Done |
| FR-3.6 | Background subagent spawning | Done |
| FR-3.7 | Cron job scheduling (interval, one-shot, cron expressions) | Done |
| FR-3.7b | Cron enable/disable | Done |
| FR-3.8 | Tool policies (allow/deny, wildcard, deny-wins) | Done |
| FR-3.9 | Tool groups (group:fs, group:runtime, group:web, etc.) | Done |
| FR-3.10 | Provider-specific tool policies (tools.byProvider) | Done |
| FR-3.11 | MCP server support (JSON-RPC stdio) | Done |
| FR-3.12 | Browser automation (headless Chrome via CDP, feature-gated) | Done |
| FR-3.13 | Diff edit (structural code editing) | Done |
| FR-3.14 | Deep search (multi-angle research tool) | Done |
| FR-3.15 | Save/recall memory tools (save_memory, recall_memory) | Done |
| FR-3.16 | Send file tool | Done |
| FR-3.17 | Switch model tool (runtime model switching) | Done |
| FR-3.18 | Run pipeline tool | Done |
| FR-3.19 | Configure tool (runtime config adjustment) | Done |

### FR-4: Gateway & Channels

| ID | Requirement | Status |
|----|-------------|--------|
| FR-4.1 | CLI channel (interactive) | Done |
| FR-4.2 | Telegram channel | Done |
| FR-4.3 | Discord channel | Done |
| FR-4.4 | Slack channel (Socket Mode) | Done |
| FR-4.5 | WhatsApp channel (Node.js bridge) | Done |
| FR-4.6 | Feishu/Lark channel | Done |
| FR-4.7 | Access control (allowed_senders) | Done |
| FR-4.8 | Session management per channel:chat_id | Done |
| FR-4.9 | Outbound message dispatch | Done |
| FR-4.10 | Email channel (IMAP/SMTP) | Done |
| FR-4.11 | Media download from channels | Done |
| FR-4.12 | Voice transcription (Groq Whisper) | Done |
| FR-4.13 | Vision support (image to LLM) | Done |
| FR-4.14 | Message coalescing (channel-aware chunking) | Done |
| FR-4.15 | Session forking (/new command) | Done |
| FR-4.16 | WeCom channel | Done |

### FR-5: Memory & Context

| ID | Requirement | Status |
|----|-------------|--------|
| FR-5.1 | Episodic memory (redb) | Done |
| FR-5.2 | Long-term memory (MEMORY.md) | Done |
| FR-5.4 | Daily notes (YYYY-MM-DD.md) | Done |
| FR-5.5 | Recent memory (7-day window) | Done |
| FR-5.6 | Bootstrap files (AGENTS.md, SOUL.md, USER.md, TOOLS.md, IDENTITY.md) | Done |
| FR-5.7 | Skills system (SKILL.md with YAML frontmatter) | Done |
| FR-5.8 | Built-in skills (3 system skills + 8 bundled app-skills) | Done |
| FR-5.9 | Hybrid memory search (BM25 + vector with HNSW) | Done |
| FR-5.10 | Context compaction (token-aware message summarization) | Done |

### FR-6: Infrastructure

| ID | Requirement | Status |
|----|-------------|--------|
| FR-6.1 | Heartbeat service (periodic HEARTBEAT.md check) | Done |
| FR-6.2 | Cron scheduler (every/at/cron expression schedules) | Done |
| FR-6.5 | Config migration framework (versioned) | Done |
| FR-6.3 | Message bus (mpsc channels) | Done |
| FR-6.4 | Session persistence (JSONL) | Done |
| FR-6.6 | Config hot-reload (SHA-256 change detection) | Done |
| FR-6.7 | Sandbox isolation (bwrap, macOS sandbox-exec, Docker) | Done |
| FR-6.8 | Environment variable sanitization (BLOCKED_ENV_VARS) | Done |

### FR-7: CLI & UX

| ID | Requirement | Status |
|----|-------------|--------|
| FR-7.1 | `crew chat` - interactive conversation | Done |
| FR-7.2 | `crew chat --message` - single-message mode | Done |
| FR-7.3 | `crew gateway` - multi-channel daemon | Done |
| FR-7.4 | `crew init` - workspace setup with bootstrap files | Done |
| FR-7.5 | `crew status` - system status | Done |
| FR-7.6 | `crew clean` - cleanup database files | Done |
| FR-7.9 | Shell completions (bash/zsh/fish/powershell) | Done |
| FR-7.10 | `crew cron` - cron job management (list/add/remove/enable) | Done |
| FR-7.11 | `crew channels status` - channel status display | Done |
| FR-7.12 | `crew auth` - OAuth login (PKCE, device code, paste-token) | Done |
| FR-7.13 | `crew skills` - skill install from GitHub | Done |
| FR-7.14 | `crew channels login` - WhatsApp QR login | Done |
| FR-7.15 | `crew office` - office automation command | Done |
| FR-7.16 | `crew pipeline` - pipeline orchestration | Done |

### FR-8: Admin & Operations

| ID | Requirement | Status |
|----|-------------|--------|
| FR-8.1 | Admin API tools (17 admin tools for profile/gateway management) | Done |
| FR-8.2 | Self-updater (in-place binary update) | Done |
| FR-8.3 | Cron timezone support | Done |
| FR-8.4 | Dashboard auth (email OTP) | Done |

## Non-Functional Requirements

| ID | Requirement | Target | Status |
|----|-------------|--------|--------|
| NFR-1.1 | CLI startup time | < 50ms | Met |
| NFR-1.2 | Memory usage (idle) | < 50MB | Met |
| NFR-2.1 | Session persistence durability | 100% on clean shutdown | Met |
| NFR-2.2 | Retry success rate | > 95% | Met |
| NFR-3.1 | No secrets in config files | Required | Met |
| NFR-3.2 | Shell command policy | SafePolicy implemented | Met |
| NFR-3.3 | Sandbox isolation | bwrap/macOS/Docker | Met |
| NFR-3.4 | Environment sanitization | 18 blocked vars | Met |
| NFR-3.5 | Path injection prevention | Per-backend validation | Met |
| NFR-4.1 | Linux/macOS support | Required | Met |
| NFR-4.2 | Rust 1.85.0 MSRV | Required | Met |
| NFR-5.1 | Docker deployment support | Required | Met |

## Technology Stack

- **Language**: Rust 2024 Edition (MSRV 1.85.0)
- **Async**: Tokio
- **HTTP**: Reqwest with rustls (no OpenSSL)
- **Database**: redb (embedded)
- **CLI**: Clap 4
- **Readline**: rustyline
- **Channels**: teloxide, serenity, tokio-tungstenite, async-imap, lettre
- **Browser**: Chrome DevTools Protocol via tokio-tungstenite, which (Chrome discovery)
- **Auth**: sha2, open (browser), base64
- **Errors**: eyre/color-eyre

## Roadmap

### Completed
- [x] Core type system and task model
- [x] 4 native LLM providers + 8 OpenAI-compatible
- [x] 25+ built-in tools
- [x] 8 messaging channels (CLI + 7 platforms)
- [x] Memory system (episodic + daily + long-term + bootstrap)
- [x] Skills system
- [x] Cron scheduler and heartbeat service
- [x] Interactive chat with readline
- [x] System status command
- [x] Provider auto-detect
- [x] Cron expression support (`"0 9 * * *"`)
- [x] Cron CLI subcommands (list/add/remove/enable)
- [x] Channels CLI subcommands (status)
- [x] Built-in skills (3 system + 8 app-skills)
- [x] Config migration framework

- [x] Media download from channels (Telegram, Discord, Slack)
- [x] Vision support (Anthropic, OpenAI, Gemini, OpenRouter)
- [x] Voice transcription (Groq Whisper)
- [x] OAuth login (`crew auth` with PKCE, device code, paste-token)
- [x] Skill install from GitHub (`crew skills install`)
- [x] Email channel (IMAP/SMTP)
- [x] WhatsApp QR login (`crew channels login`)
- [x] Docker deployment (multi-stage Dockerfile + docker-compose)
- [x] Tool policies (allow/deny, groups, provider-specific)
- [x] Context compaction (token-aware message summarization)
- [x] Config hot-reload (SHA-256 change detection)
- [x] Hybrid memory search (BM25 + vector)
- [x] Message coalescing (channel-aware chunking)
- [x] Session forking (`/new` command)
- [x] Docker sandbox (container isolation with resource limits)
- [x] MCP server support (JSON-RPC stdio + HTTP/SSE)
- [x] Streaming responses (chat_stream + SSE)
- [x] Hook/lifecycle system (4 events, circuit breaker)
- [x] Provider failover chain (ProviderChain with circuit breaker)
- [x] Built-in web UI (embedded SPA via rust-embed)
- [x] Prometheus metrics endpoint (/metrics)
- [x] Message queue modes (Followup vs Collect)
- [x] Wall-clock agent timeout (600s default)
- [x] Tool output sanitization (strip base64/hex)
- [x] `secrecy::SecretString` for all API keys in memory

- [x] Comprehensive security audit (30 findings fixed, 316 tests)
- [x] Symlink-safe file I/O (O_NOFOLLOW on Unix)
- [x] Shared SSRF protection module
- [x] MCP schema validation (depth/size limits)
- [x] Tool argument size limits (1MB)
- [x] Session file size limits (10MB)
- [x] API default bind to 127.0.0.1
- [x] Configurable hybrid search weights
- [x] Shell timeout bounds (1-600s)

### Planned
- [ ] DingTalk, QQ channels
