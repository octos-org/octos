# Octos vs OpenClaw: Comprehensive Feature Comparison
## 30-Slide PPTX Outline

---

### Slide 1: Title
**Octos vs OpenClaw: Two Paths to Personal AI**
- Octos: Rust-native, multi-tenant, self-healing agentic framework
- OpenClaw: TypeScript/Node.js, single-user personal AI assistant
- Subtitle: Architecture, Performance, Security, and Ecosystem Comparison

---

### Slide 2: Philosophy & Vision
| | Octos | OpenClaw |
|---|---|---|
| **Metaphor** | Octopus — 8 arms think independently, one shared brain. Self-healing (regenerates limbs), camouflage (adapts). Self-evolving agentic system. | Lobster — "EXFOLIATE!" Personal shell that protects you. |
| **Design goal** | Multi-tenant AI agent platform for teams & orgs | Single-user personal AI assistant |
| **Core principle** | Multi-provider resilience + tenant isolation | Privacy-first, runs on your devices |
| **Language** | Rust (pure, single binary, no runtime) | TypeScript/Node.js (npm ecosystem) |

---

### Slide 3: Architecture Overview
**Octos**: 6-crate Rust workspace, layered
```
octos-cli → octos-agent → octos-llm / octos-memory → octos-core
                        ↘ octos-bus (channels, sessions)
                        ↘ octos-pipeline (DOT orchestration)
```

**OpenClaw**: Monorepo with workspaces
```
CLI → Gateway (WS) → Pi Agent RPC
  ├─ 72 extension packages
  ├─ 47 bundled skills
  ├─ Native apps (macOS/iOS/Android)
  └─ Canvas Host (A2UI)
```

---

## Category 1: Onboarding Experience (Slides 4-7)

### Slide 4: Installation
| | Octos | OpenClaw |
|---|---|---|
| **Install method** | `cargo install` or single binary copy | `npm install -g openclaw` or `npx openclaw` |
| **Dependencies** | Zero runtime deps (pure Rust, static binary) | Node.js 22+, npm/pnpm, optional: Chromium, ffmpeg |
| **Binary size** | ~45MB single binary | ~200MB+ (node_modules + runtime) |
| **First run** | `octos init --defaults` | `openclaw onboard` (guided wizard) |
| **Time to first chat** | ~2 min (set API key, run) | ~5 min (wizard walks through everything) |
| **Guided setup** | Manual config.json | Interactive `openclaw onboard` wizard |

### Slide 5: Deployment
| | Octos | OpenClaw |
|---|---|---|
| **On-premise** | Single binary + launchd/systemd | npm install + launchd/systemd |
| **Docker** | Multi-stage Alpine, ~24MB runtime | Multi-stage, ~300MB+ runtime |
| **Cloud** | Any Linux/macOS VM (copy binary) | Docker/Nix/K8s, 25+ platform guides |
| **Self-hosting tunnel** | frp scripts (PR #31) + Caddy | Tailscale Funnel built-in |
| **Raspberry Pi** | Cross-compile to ARM64 | ARM64 Node.js |
| **Windows** | Native support (PR #27) + WSL2 | WSL2 strongly recommended |

### Slide 6: Configuration
| | Octos | OpenClaw |
|---|---|---|
| **Config format** | JSON (`~/.octos/config.json`) | JSON (`~/.openclaw/openclaw.json`) |
| **Secrets** | macOS Keychain + env vars | `.env` files + credentials dir |
| **Hot reload** | System prompt only (SHA-256 poll) | System prompt only (SHA-256 poll) |
| **Validation** | Runtime checks + warnings | Zod schema validation |
| **Migration** | Versioned config migration | Auto-migration |

### Slide 7: User Management
| | Octos | OpenClaw |
|---|---|---|
| **Multi-tenant** | Yes — separate gateway per profile, shared dashboard | No — single user per gateway |
| **User auth** | Admin token + OTP email + user sessions | Device pairing + challenge signature |
| **Profile management** | Web dashboard + CLI + API | CLI only |
| **Sub-accounts** | Yes — parent/child profiles sharing config | No |
| **Admin dashboard** | Full React SPA (profiles, metrics, skills, logs) | Control plane (basic status) |

---

## Category 2: Developer Experience (Slides 8-11)

### Slide 8: Contributing to Core
| | Octos | OpenClaw |
|---|---|---|
| **Language** | Rust (steep learning curve, high ceiling) | TypeScript (lower barrier, wider pool) |
| **Build time** | ~2 min full release build | ~30s incremental, ~2 min full |
| **Test suite** | 1,555 tests (15s unit, 5min integration) | 2,921 test files (Vitest) |
| **CI** | GitHub Actions (macOS-14 runner) | GitHub Actions (multi-platform) |
| **TDD** | Required (RED→GREEN→REFACTOR in CLAUDE.md) | Vitest + coverage thresholds (70%) |
| **Code style** | cargo fmt + clippy | oxlint + prettier + markdownlint |

### Slide 9: Skill/Plugin Development
| | Octos | OpenClaw |
|---|---|---|
| **Plugin protocol** | stdin/stdout JSON (any language) | TypeScript SDK (170+ exports) |
| **Skill format** | SKILL.md (markdown + YAML frontmatter) | `openclaw.plugin.json` manifest |
| **Tool definition** | manifest.json (JSON Schema) | TypeScript + Zod schemas |
| **Binary distribution** | Pre-built binaries with SHA-256 | npm packages |
| **Registry** | octos-hub (GitHub-based, manual PR) | ClawHub (centralized registry) |
| **Per-profile install** | Yes — CLI, API, in-chat, agent tool | No — global only |
| **Asset bundling** | Self-contained in skill dir | npm package structure |
| **Bundled skills** | 8 app-skills + 3 built-in | 47 bundled skills |

### Slide 10: Channel Development
| | Octos | OpenClaw |
|---|---|---|
| **Channel trait** | Rust `Channel` trait (start, send, edit, delete) | TypeScript SDK (inbound, outbound, lifecycle) |
| **Built-in channels** | 11 (Telegram, Discord, Slack, WhatsApp, Feishu, Email, WeCom, Twilio, API, CLI, WeCom Bot) | 8 built-in + 21 extension channels = 29 |
| **Crypto** | Pure Rust (no OpenSSL) — SHA-1, SHA-256, AES-CBC, HMAC | Node.js crypto module |
| **Adding a channel** | Implement trait, add feature flag | npm package with SDK imports |
| **Webhook proxy** | Built-in (Caddy routing by profile) | Built-in webhook ingress |

### Slide 11: SDK & Extension Points
| | Octos | OpenClaw |
|---|---|---|
| **SDK exports** | Rust crate API (octos-agent, octos-llm) | 170+ SDK module paths |
| **MCP support** | JSON-RPC stdio transport | @modelcontextprotocol/sdk v1.27 |
| **Hooks** | 4 events (before/after tool/LLM) with deny | Before/after tool/LLM hooks |
| **Prompt fragments** | manifest.json `prompts.include` globs | Skill markdown injection |
| **Custom providers** | Implement `LlmProvider` trait | Provider extension package |

---

## Category 3: Features (Slides 12-18)

### Slide 12: Tool Use
| | Octos | OpenClaw |
|---|---|---|
| **Core tools** | 14 built-in + 20 specialized | 46+ core tools |
| **Shell execution** | SafePolicy (deny rm -rf, dd, mkfs, fork bomb) | Sandbox-gated exec |
| **File I/O** | read/write/edit/diff_edit + glob + grep | read/write + file system tools |
| **Browser** | Headless Chrome via CDP | Playwright Core + NoVNC fallback |
| **Concurrent tools** | All tools in one iteration run via `join_all()` | Sequential within iteration |
| **Sub-agents** | Sync + background spawn | Session-based agent routing |
| **Pipeline** | DOT-based multi-node orchestration with dynamic_parallel | No equivalent |
| **Tool argument limit** | 1MB (non-allocating size estimation) | 1MB |

### Slide 13: Search & Research
| | Octos | OpenClaw |
|---|---|---|
| **Web search** | DuckDuckGo → Exa → Brave → You.com → Perplexity (failover chain) | Brave, Perplexity, Tavily, Firecrawl |
| **Deep search** | Parallel multi-query (6 concurrent workers) | Playwright-based deep crawl |
| **Deep research** | Pipeline-based: plan → search → analyze → synthesize (DOT graph) | Document crawl + synthesis |
| **Content extraction** | deep_crawl + site_crawl tools | web-fetch + browser snapshot |
| **Citation tracking** | Pipeline node summaries | Built-in citation provenance |

### Slide 14: Content Generation
| | Octos | OpenClaw |
|---|---|---|
| **PPTX** | Native (zip + XML, no external deps) | No native support |
| **DOCX** | Native | No native support |
| **XLSX** | Native | No native support |
| **PDF** | LibreOffice conversion + pdftoppm | pdfjs-dist extraction |
| **Image gen** | Via skills (mofa-cards, mofa-comic, mofa-infographic) | DALL-E, FAL.ai, Midjourney, Stable Diffusion |
| **Image analysis** | Provider vision (auto-strips for non-vision models) | Provider vision (same) |
| **Comics/Infographics** | mofa-comic (6 styles), mofa-infographic (4 styles) | No equivalent |
| **Slides with AI images** | mofa-slides (17 styles, Gemini image gen) | No equivalent |

### Slide 15: Voice & Media
| | Octos | OpenClaw |
|---|---|---|
| **TTS** | Qwen3-TTS via voice-skill (voice cloning, emotion, speed control) | ElevenLabs, Deepgram, system TTS |
| **ASR/Transcription** | Groq Whisper | OpenAI Whisper, Deepgram, local ONNX |
| **Voice cloning** | X-vector profiles | ElevenLabs voice cloning |
| **Voice wake** | No | macOS/iOS wake word detection |
| **Talk mode** | No | Continuous voice (Android/macOS) |
| **Voice calls** | No | WebRTC real-time calls |
| **Media handling** | Image/audio/video via channels | Rich media pipeline |

### Slide 16: GitHub & Dev Tools
| | Octos | OpenClaw |
|---|---|---|
| **PR review** | No built-in (use via agent chat) | GitHub skill (PR review + suggestions) |
| **Issue management** | `gh` CLI via shell tool | GitHub skill (CRUD) |
| **Code structure** | tree-sitter AST analysis (feature-gated) | No equivalent |
| **Git operations** | git feature-gated tool | Via shell |
| **Coding agent** | Full agent loop with tool calls | Coding agent skill |

### Slide 17: Canvas & Visual
| | Octos | OpenClaw |
|---|---|---|
| **Live canvas** | No | A2UI visual workspace (HTML/CSS/JS) |
| **Agent-driven UI** | Web dashboard (read-only) | Real-time push/reset/eval |
| **Interactive components** | No | Buttons, inputs, dialogs |
| **Screenshot** | Browser tool screenshot | Canvas + browser snapshot |

### Slide 18: IoT & Device Integration
| | Octos | OpenClaw |
|---|---|---|
| **Native apps** | No (server-only) | macOS, iOS, Android companion apps |
| **Device commands** | No | camera.snap, screen.record, location.get, SMS, contacts |
| **Smart home** | No built-in | OpenHue (Philips Hue) skill |
| **Node mesh** | No | Device nodes connect via WebSocket |
| **Bonjour discovery** | No | iOS local network discovery |

---

## Category 4: Performance (Slides 19-21)

### Slide 19: Runtime Performance
| | Octos | OpenClaw |
|---|---|---|
| **Language overhead** | Zero (native machine code, no GC) | V8 JIT + GC pauses |
| **Binary size** | ~45MB single static binary | ~200MB+ (node_modules) |
| **Startup time** | <100ms | ~500ms CLI, ~2s gateway |
| **Memory baseline** | ~20MB per gateway | ~200-300MB per gateway |
| **Per-session overhead** | ~few KB (tokio green thread) | ~10-20MB (V8 isolate) |
| **True parallelism** | Yes (multi-core via tokio) | No (single-threaded event loop + worker_threads) |
| **GC pauses** | None (deterministic deallocation) | V8 GC (unpredictable latency spikes) |

### Slide 20: Reliability & Fault Tolerance
| | Octos | OpenClaw |
|---|---|---|
| **Provider failover** | RetryProvider → ProviderChain → AdaptiveRouter (3 layers) | Multi-key + model fallback |
| **Adaptive routing** | Hedge mode (race 2 providers), Lane mode (switch on degradation), QoS ranking | No equivalent |
| **Circuit breaker** | Auto-disable degraded providers (3+ failures) | No |
| **Responsiveness** | EMA latency tracking, P95 degradation detection | No |
| **Stream timeout** | 30s per-chunk timeout | Connection-level timeout |
| **Process isolation** | Separate OS process per profile | Single process |
| **Crash recovery** | launchd KeepAlive, atomic session writes | launchd/systemd restart |

### Slide 21: Multi-Tenant Density
| | Octos | OpenClaw |
|---|---|---|
| **Architecture** | One gateway process per profile (~20MB each) | One gateway per user (~300MB) |
| **Profiles on 8GB Mac Mini** | ~200+ concurrent profiles | ~20 instances |
| **Concurrent sessions** | Thousands (tokio green threads, ~KB each) | Hundreds (V8 isolates, ~MB each) |
| **Semaphore control** | Per-profile configurable (default 10) | No per-user limiting |
| **Resource sharing** | Shared binary, separate data dirs | Separate Node.js processes |

---

## Category 5: Security (Slides 22-25)

### Slide 22: Execution Sandbox
| | Octos | OpenClaw |
|---|---|---|
| **Bwrap (Linux)** | Yes — RO bind, RW workdir, unshare-pid/network | Yes |
| **macOS sandbox-exec** | Yes — SBPL profile, kernel enforcement | Yes |
| **Docker** | Yes — no-new-privileges, cap-drop ALL, resource limits | Yes — mount modes, network isolation |
| **Windows** | NoSandbox fallback (cmd /C) | WSL2 sandbox |
| **Path injection** | Rejects `:`, `\0`, `\n`, `\r`, control chars, `(`, `)` | Similar validation |

### Slide 23: User & Data Isolation
| | Octos | OpenClaw |
|---|---|---|
| **Profile isolation** | Separate OS process, own data dir, own API keys | Single user per gateway |
| **Session isolation** | Per-session actor, Mutex serialization | Per-session isolation |
| **Personal data** | Per-profile data dir, per-user session paths | Per-gateway data dir |
| **Cross-profile access** | Impossible (separate processes) | N/A (single user) |
| **Sub-account isolation** | Shares parent skills, own data dir | No sub-accounts |

### Slide 24: Tool & Prompt Security
| | Octos | OpenClaw |
|---|---|---|
| **SSRF protection** | Blocks private IPs, IPv6 ULA/link-local | Same |
| **Symlink safety** | O_NOFOLLOW on Unix (TOCTOU prevention) | Similar |
| **Env sanitization** | 18 BLOCKED_ENV_VARS (LD_PRELOAD, DYLD_*, etc.) | Same constant |
| **Shell SafePolicy** | Deny rm -rf /, dd, mkfs, fork bomb; ask for sudo | Sandbox-gated |
| **Tool argument limit** | 1MB (non-allocating estimation) | 1MB |
| **MCP schema validation** | Max depth 10, max size 64KB | SDK validation |
| **Prompt injection** | 73 unit tests for DAN/jailbreak/role confusion | Model-selection guidance |
| **Unsafe code** | `#![deny(unsafe_code)]` workspace-wide | N/A (TypeScript) |

### Slide 25: Credential Management
| | Octos | OpenClaw |
|---|---|---|
| **Key storage** | macOS Keychain (via `security` CLI) | `.env` files + credentials dir |
| **OAuth** | PKCE with SHA-256 challenges | Provider-specific OAuth |
| **Token comparison** | Constant-time byte comparison (timing attack prevention) | Standard comparison |
| **API key wrapping** | `secrecy::SecretString` (prevents logging) | Env var masking |
| **Auth store** | `~/.octos/auth.json` (mode 0600) | `~/.openclaw/credentials/` |

---

## Category 6: Memory & Data Architecture (Slide 26)

### Slide 26: Memory System
| | Octos | OpenClaw |
|---|---|---|
| **Episode store** | redb (embedded key-value, ACID) | JSONL files |
| **Vector search** | HNSW index (hnsw_rs, 16 connections, 10K capacity) | LanceDB (embedded vectorDB) |
| **Hybrid search** | BM25 (K1=1.2, B=0.75) + cosine similarity (0.7/0.3) | BM25 + cosine (LanceDB) |
| **Long-term memory** | MEMORY.md + daily notes (7-day window) | LanceDB persistent store |
| **Session persistence** | JSONL with LRU cache, atomic write-then-rename | JSONL with agent-scoped dirs |
| **Context compaction** | Token-aware: strip tool args, summarize, preserve recent pairs | Token-aware summarization |
| **Embedding provider** | OpenAI text-embedding-3-small (1536d) | Configurable (OpenAI default) |
| **Fallback** | BM25-only without embedding provider | BM25-only fallback |

---

## Category 7: User Contribution & SDK (Slide 27)

### Slide 27: Extension Ecosystem
| | Octos | OpenClaw |
|---|---|---|
| **SDK surface** | Rust crate APIs (compile-time safety) | 170+ TypeScript SDK module paths |
| **Plugin protocol** | stdin/stdout JSON (language-agnostic) | TypeScript-only (SDK imports) |
| **Skill count** | 8 bundled + community via octos-hub | 47 bundled + 72 extensions |
| **Registry** | octos-hub (GitHub, manual PR review) | ClawHub (centralized, automated) |
| **Skill creation** | `skill-creator` built-in skill teaches agent to create skills | `skill-creator` skill |
| **MCP integration** | Manifest-declared MCP servers (stdio + HTTP) | @modelcontextprotocol/sdk |
| **Lifecycle hooks** | 4 events, manifest-declared, per-skill | Before/after hooks |
| **Contribution guide** | CLAUDE.md + TDD rules | CONTRIBUTING.md + AGENTS.md (170 lines) |

---

## Category 8: Channel Support (Slide 28)

### Slide 28: Messaging Platform Coverage
| Platform | Octos | OpenClaw |
|----------|-------|----------|
| Telegram | Built-in | Built-in + extension |
| WhatsApp | Built-in (Baileys bridge) | Built-in (Baileys) |
| Discord | Built-in | Built-in + extension |
| Slack | Built-in | Built-in + extension |
| Feishu/Lark | Built-in (WS + webhook, AES-256) | Extension |
| WeCom | Built-in (API + Bot, pure Rust crypto) | No |
| Twilio (SMS) | Built-in (webhook, HMAC-SHA1) | No |
| Email | Built-in (IMAP + SMTP) | No |
| API (HTTP/SSE) | Built-in (REST + SSE streaming) | WebChat |
| CLI | Built-in (readline) | Built-in |
| Google Chat | No | Built-in + extension |
| Signal | No | Extension |
| iMessage | No | Extension (BlueBubbles) |
| IRC | No | Extension |
| LINE | No | Extension |
| Matrix | No | Extension |
| Microsoft Teams | No | Extension |
| Mattermost | No | Extension |
| Nextcloud Talk | No | Extension |
| Nostr | No | Extension |
| Synology Chat | No | Extension |
| Tlon | No | Extension |
| Twitch | No | Extension |
| Zalo | No | Extension |
| QQ Bot | PR #2 (pending) | No |
| **Total** | **11 + 1 pending** | **29** |

---

### Slide 29: Competitive Advantages
**Octos strengths:**
- Pure Rust: zero-GC, true parallelism, ~45MB binary, ~20MB per tenant
- Multi-tenant: 200+ profiles on one Mac Mini vs ~20 for Node.js
- Adaptive routing: hedge/lane/QoS with circuit breaker — no equivalent in OpenClaw
- Pipeline orchestration: DOT-based multi-node research with dynamic parallelism
- Office suite: native PPTX/DOCX/XLSX generation (no external deps)
- mofa-skills: AI comic/infographic/slide generation with style system
- Enterprise security: 73 prompt injection tests, constant-time auth, Keychain integration
- Per-profile skill isolation: binary stays in profile dir, no global pollution

**OpenClaw strengths:**
- 29 channels vs 11 — broader messaging platform reach
- Native mobile apps (macOS/iOS/Android) — device mesh with camera/GPS/contacts
- Canvas (A2UI) — live visual workspace controlled by agent
- Voice: wake word, talk mode, WebRTC calls — no equivalent in octos
- 170+ SDK module paths — richer plugin development surface
- Guided onboarding wizard — better first-run experience
- Larger test suite (2,921 files vs 1,555 tests)
- npm distribution — easier install for Node.js developers

---

### Slide 30: Summary & Recommendation
| Use Case | Recommended |
|----------|-------------|
| **Multi-tenant SaaS** | Octos (process isolation, 10x density) |
| **Enterprise deployment** | Octos (security, Keychain, sandbox, audit) |
| **Personal AI assistant** | OpenClaw (mobile apps, voice, canvas) |
| **Content creation (office)** | Octos (native PPTX/DOCX/XLSX, mofa-skills) |
| **Deep research** | Octos (pipeline orchestration, multi-provider) |
| **Plugin ecosystem** | OpenClaw (170+ SDK paths, npm distribution) |
| **Channel breadth** | OpenClaw (29 vs 11 platforms) |
| **Performance-critical** | Octos (Rust, zero-GC, true parallelism) |
| **IoT/Device mesh** | OpenClaw (native apps, camera, GPS) |
| **Self-hosting (minimal)** | Octos (single 45MB binary, zero deps) |
