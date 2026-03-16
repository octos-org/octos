# Feature Parity: octos vs Moltis

Comparison of [octos](https://github.com/heyong4725/octos) (6-crate workspace, coding agent CLI + gateway) against [Moltis](https://github.com/moltis-org/moltis) (27-crate workspace, local-first AI gateway with web UI).

Both are Rust, single-binary, multi-provider, sandboxed agent frameworks.

---

## Features octos ALREADY HAS (parity)

| Feature | octos | Moltis |
|---|---|---|
| Multi-provider LLM | 4 native + 8 compatible (12 total) | 16+ providers |
| Streaming SSE | Per-provider parsers | WebSocket streaming |
| Tool system | 14 built-in tools (13 default + browser) | Similar set |
| Sandbox | bwrap / sandbox-exec / Docker | Docker / Apple Container |
| MCP support | JSON-RPC stdio + HTTP/SSE | stdio + HTTP/SSE |
| Skills system | SKILL.md + built-ins | SKILL.md + built-ins |
| Plugin system | manifest.json + executable | Similar |
| Session persistence | JSONL + in-memory cache | JSONL + SQLite metadata |
| Memory/embeddings | BM25 + HNSW hybrid search | SQLite vector + full-text |
| Cron/scheduling | CronService with at/every/cron | Similar |
| Channels | CLI, Telegram, Discord, Slack, WhatsApp, Feishu, Email (7) | Web, Telegram, Discord (3) |
| Sub-agent spawning | spawn tool (sync + background) | spawn_agent tool |
| Context compaction | 80% threshold, summary | 95% threshold, summary |
| Retry with backoff | RetryProvider (3 retries) + ProviderChain failover | ProviderChain failover |
| Tool policies | allow/deny with groups | Hook-based gating |
| SSRF protection | DNS resolution + private IP blocking | DNS resolution + IP blocking |
| Shell safety | SafePolicy deny/ask patterns | Hook-based blocking |
| Audio transcription | Groq Whisper | STT providers |
| Image/vision | Base64 encoding per provider | Similar |
| Config hot-reload | SHA-256 polling, 5s | Similar |
| Parallel tool execution | `futures::join_all` for concurrent tool calls | `futures::join_all` |
| Wall-clock agent timeout | 600s default via `tokio::time::Instant` | 600s hard timeout |
| Tool output sanitization | Strip base64 data URIs + long hex strings | Strip base64/hex/secrets |
| `secrecy::SecretString` | All provider API keys wrapped | Secrets wrapped |
| `#![deny(unsafe_code)]` | Workspace-wide lint | Workspace-wide |

---

## Features Moltis HAS that octos LACKS

| # | Feature | Moltis Implementation | Improvement Potential for octos |
|---|---|---|---|
| 1 | ~~**Hook/Lifecycle System**~~ | 17 lifecycle events (BeforeToolCall, BeforeLLMCall, MessageSending, etc.). Sequential for modifying, parallel for read-only. Shell protocol (JSON stdin, exit code + stdout). Circuit breaker (3 failures -> auto-disable). HOOK.md discovery. | **DONE** -- 4 events (before/after tool call, before/after LLM call). Shell protocol (JSON stdin, exit codes 0/1/2+). Circuit breaker with configurable threshold. Tool filtering. Env sanitization via BLOCKED_ENV_VARS. Wired into chat, gateway, serve. Config hot-reload aware. |
| 2 | ~~**Built-in Web UI**~~ | SPA embedded via `include_dir!()`. WebSocket streaming. Settings panel. Hook editor. Session browser. | **DONE** -- Embedded SPA via `rust-embed` at `/` with session sidebar, chat, SSE streaming, dark theme. |
| 3 | **WebAuthn / Passkey Auth** | FIDO2 credentials (Touch ID, security keys). Stored in SQLite. | **LOW** -- octos targets CLI/bot use cases where passkeys are less relevant. |
| 4 | **Apple Container** (macOS native containers) | Native macOS containerization beyond sandbox-exec. | **LOW** -- octos already has sandbox-exec. Apple Container is newer/niche. |
| 5 | ~~**Browser Automation**~~ | Playwright-based browser tool with session pool. | **DONE** -- Headless Chrome via CDP over tokio-tungstenite. Feature-gated `browser`. Actions: navigate (SSRF + scheme check), get_text, get_html, click, type, screenshot, evaluate, close. 5min idle timeout, env sanitization, 10s JS timeout, zombie reaping, secure tempfiles. |
| 6 | **TTS (Text-to-Speech)** | Multiple TTS providers (ElevenLabs, etc.). | **LOW** -- Niche for CLI/bot agent. |
| 7 | **Onboarding Wizard** | Guided first-run setup for identity, profile, personality. | **LOW** -- octos has `crew init` which is simpler but sufficient. |
| 8 | **Sandbox Image Management** | CLI commands: `sandbox list/build/clean/remove`. Deterministic image tags (hash of base + packages). Auto-rebuild on package change. | **LOW** -- Nice UX but not critical. |
| 9 | ~~**Message Queue Modes**~~ | `followup` (replay each queued message) vs `collect` (concatenate). Handles messages arriving during active agent run. | **DONE** -- `QueueMode::Followup` (FIFO) vs `QueueMode::Collect` (merge by session) via `gateway.queue_mode`. |
| 10 | ~~**Prometheus Metrics**~~ | `/metrics` endpoint, SQLite history for metrics. | **DONE** -- `/metrics` endpoint with tool call counters/histograms and LLM token counters. |
| 11 | **fd-lock for Sessions** | File-level locking prevents concurrent JSONL corruption. | **LOW** -- octos uses atomic write-then-rename which is mostly safe. |
| 12 | **Per-IP Rate Limiting** | Built-in throttling for unauthenticated traffic. `429 + Retry-After`. | **LOW** -- Only relevant for the REST API feature. |

---

## Features octos HAS that Moltis LACKS

| Feature | octos | Notes |
|---|---|---|
| **WhatsApp channel** | WebSocket bridge to Baileys | Moltis doesn't have this |
| **Slack channel** | Socket Mode + REST | Moltis doesn't have this |
| **Feishu/Lark channel** | WebSocket + REST | Moltis doesn't have this |
| **Email channel** | IMAP + SMTP | Moltis doesn't have this |
| **Heartbeat service** | Periodic HEARTBEAT.md check | Unique to octos |
| **diff_edit tool** | Unified diff with fuzzy matching | Moltis uses standard edit |
| **Pricing module** | Per-model cost tracking | Not mentioned in Moltis |
| **OAuth PKCE for OpenAI** | Browser + device code flows | Moltis has OAuth but less documented |

---

## Recommended Improvements (Prioritized)

### Tier 1 -- High Impact, Moderate Effort

1. ~~**Parallel tool execution**~~ DONE -- `futures::join_all` for concurrent tool calls.

2. ~~**Provider failover chain**~~ DONE -- `ProviderChain` with circuit breaker (degrades after 3 consecutive failures, resets on success).

3. ~~**Hook/lifecycle system**~~ DONE -- 4 events (before/after tool/LLM), shell protocol (JSON stdin, exit codes), circuit breaker, tool filtering, env sanitization, config hot-reload.

### Tier 2 -- Medium Impact

4. ~~**Wall-clock agent timeout**~~ DONE -- 600s default via `AgentConfig.max_timeout`.

5. ~~**Tool output sanitization**~~ DONE -- Strips base64 data URIs and long hex strings in `sanitize.rs`.

6. ~~**MCP HTTP/SSE transport**~~ DONE -- `McpServerConfig.url` for remote MCP servers with JSON and SSE response handling.

7. ~~**DNS-based SSRF protection**~~ DONE -- `tokio::net::lookup_host` resolves DNS and checks all IPs against private ranges.

### Tier 3 -- Low Priority / Nice-to-Have

8. ~~**`secrecy::SecretString`** for API keys~~ DONE -- All 6 provider types use `SecretString`.

9. ~~**`#![deny(unsafe_code)]`** workspace-wide~~ DONE -- Via `[workspace.lints.rust]`.

10. ~~**Built-in web UI**~~ DONE -- Embedded SPA via `rust-embed` at `/` with session sidebar, chat, SSE streaming, dark theme. Vanilla HTML/CSS/JS, no build tools.

11. ~~**Prometheus metrics endpoint**~~ DONE -- `/metrics` endpoint with `crew_tool_calls_total`, `crew_tool_call_duration_seconds`, `octos_llm_tokens_total` counters/histograms via `metrics` + `metrics-exporter-prometheus`.

12. ~~**Message queue modes**~~ DONE -- `QueueMode::Followup` (FIFO, default) vs `QueueMode::Collect` (merge queued messages by session) via `gateway.queue_mode` config field.

---

*Analysis date: 2026-02-13*
*Last updated: 2026-02-13 (12 of 12 improvements implemented -- feature parity complete)*
*Sources: [Moltis GitHub](https://github.com/moltis-org/moltis), [DeepWiki](https://deepwiki.com/moltis-org/moltis)*
