# Changelog

All notable changes to octos will be documented in this file.
## [Unreleased]

### Changed

- Per-tenant frps tunnel authentication via `metadatas.token`. Each tenant now has its own `tunnel_token` (UUID generated at registration) validated by the octos frps server plugin; the previous shared FRPS auth token is no longer needed and `auth.token` is set to `""` on both frps and frpc. `scripts/install.sh` and `scripts/install.ps1` recover the per-tenant token from an existing `/etc/frp/frpc.toml` on rerun and have updated prompt wording to reflect the per-tenant model.
- README "Quick Start" restructured into a three-step cloud-deployment walkthrough (VPS bootstrap → portal registration → tenant install) with explicit uninstall instructions for both cloud and tenant machines. The developer build flow moved under a new "Build from source" heading.

## [0.1.1] - 2026-04-07

### Highlights

- **Slides Studio** — End-to-end AI slide generation pipeline with policy-driven provider chains and task status tracking
- **Content Management** — Per-profile content catalog, directory tree browser, and workspace scanner
- **Multi-Platform Channels** — Matrix Appservice, QQ Bot (Official API v2), WeCom Group Robot WebSocket
- **Deep Search** — Exa neural search, Serper.dev, Tavily, Google CDP fallback with smart engine routing
- **Sandbox by Default** — AppContainer sandbox for Windows, per-profile isolation, sandbox enabled by default
- **Deployment** — Auto-HTTPS with `--caddy-domain`, Windows installer, tenant self-registration, cloud/local/tenant modes
- **Skill Version Check** — Pre-clone registry version comparison skips unnecessary downloads on `skills update`

### Features

- Slides studio end-to-end pipeline with policy-driven provider chain and task status checks
- Content panel with directory tree, workspace scan, and markdown viewer
- Per-profile content catalog with REST API
- Per-user soul/personality customization via /soul command and API
- AppContainer sandbox for Windows
- Per-profile sandbox isolation and skill directory layering
- Matrix Appservice channel with BotFather architecture
- QQ Bot channel with Official API v2 WebSocket gateway
- WeCom Group Robot WebSocket channel
- Discord reactions, embeds, and message dedup
- Exa neural search as top-priority web search provider
- Serper.dev as first-priority search engine in deep-search
- Tavily web search provider
- Google CDP search fallback via headless Chrome
- Smart search engine routing exposed to parent LLM
- Delta streaming for API channel (token appends instead of full replace)
- SSE progress events forwarded through API channel for web client
- MSC4357 live message markers for streaming edits
- Auto-HTTPS with `--caddy-domain` and on-demand TLS
- Windows install.ps1 auto-installs deps, Caddy, and firewall rules
- Playwright e2e testing via `--test` flag in deploy.sh
- Tenant self-registration with POST /api/register and setup scripts
- Cloud/local/tenant deployment modes via config.json
- spawn_only tools — deferred in main session, available in subagents
- Auto-redirect spawn_only tools to background spawn with retry and notify lifecycle
- spawn_only_message configurable per tool in manifest.json
- Universal auto-send hook — detect and deliver files from any plugin output
- Two-tier deferred tool dispatch with activate_tools meta-tool
- Composable multi-layer status system with per-user config
- Bot owner and visibility model with default-private enforcement
- Message metadata annotations and QoS model catalog
- Profile-scoped routing and sender identity infrastructure
- Pipeline executor observability and model catalog baseline
- API channel file download endpoint and SSE file events
- Pipeline-guard hook owns model selection
- Unified QoS model catalog as single source of truth
- Default to top-2 engine racing instead of single-best
- Native Windows support
- Dashboard skills page and sidebar refactor
- Per-profile sandbox isolation and skill directory layering
- Plugin loader returns MCP servers, hooks, and prompt fragments from skills
- Version check on skill install, add update action
- Pre-clone version check for skill updates — skip clone if already up to date
- Deep-search saves to OCTOS_WORK_DIR, agent sends report via send_file
- HTML boilerplate cleaning, adaptive stream timeout, GLM-5 provider
- Voice cloning with x-vector profiles
- Streaming support for WeCom bot channel
- Persist OTP auth sessions to disk across server restarts

### Security

- SSRF redirect bypass and DNS failure fallthrough hardened in web_fetch
- CORS wildcard replaced with explicit origin allowlist
- Path traversal in hook tilde expansion validated
- admin_shell endpoint disabled by default via config flag
- X-Profile-Id auth restricted to loopback origin
- Sandbox enabled by default (SandboxMode::Auto)
- Spawn tool restricted to append-only prompt instructions
- Sensitive data redacted from hook payloads
- Send_file path validation prevents cross-profile file exfiltration

### Bug Fixes

- Loop detection breaks agent loop instead of just warning
- Stronger spawn_only message to prevent LLM retry loops
- Model-specific max_output_tokens defaults instead of 8192
- SSE byte-buffer prevents UTF-8 corruption of CJK characters
- Concurrency cap added to pipeline parallel fan-out
- Global timeout cap added to ProviderChain (default 120s)
- Process allocation race in ProcessManager
- HNSW capacity fallback to BM25-only search
- Eliminate production unwrap/expect calls
- Report_late_failure penalizes correct provider slot
- Wrap blocking I/O in spawn_blocking (cron_service, session)
- Plugin auto-deliver checks work_dir, cwd, and output text
- SSE grace period now triggers when spawn_only tools exist
- Upload body limit raised to 100MB for file attachments
- Forward all non-audio media to agent
- Content catalog only scans profile data_dir
- Deferred file events for web clients when SSE connection is closed

### Infrastructure

- Version management with cargo-release and git-cliff
- GitHub Actions bumped: checkout@v6, upload-artifact@v7, download-artifact@v8, setup-node@v6
- Caddy config updated to proxy all requests to octos serve
- Cloud host deploy script and local-tenant-deploy.sh added

## [0.1.0] - 2026-03-05

### Bug Fixes

- Address critical and high review findings in streaming code
- Shutdown check during streaming, buffered stdout, robust session keys
- Remaining review items — retry, error truncation, atomicity, CJK
- Address 18 security and quality review findings
- Reject absolute paths, expand env blocklist, add security tests
- Close path traversal in list_dir and glob, harden sandbox
- Add symlink checks, SSRF protection, glob traversal, spawn depth
- Close IPv6 SSRF bypass in web_fetch private host check
- Block site-local and IPv4-compatible IPv6 in SSRF filter
- Block IPv6 multicast in SSRF filter
- Handle RwLock poisoning, eliminate TOCTOU, hash all config files
- Eliminate duplicate index entries and log poisoned locks
- Validate embedding dimensions and prevent UTF-8 slice panic
- Enforce provider policy at execution time and propagate to subagents
- Harden sandbox path validation and session file uniqueness
- Address remaining review findings across sandbox, coalesce, session
- Unify env blocklist, add UTF-8 safe truncation, improve error handling
- Block sandbox injection via newlines and SBPL parens, extract truncate_utf8
- Reject backslash and quote in macOS sandbox paths
- Prevent process leak and stdin race in hook executor
- Improve hook robustness for circuit breaker, success, and denials
- Harden browser tool with 6 security and quality fixes
- Resolve 8 audit issues across security, correctness, and perf
- Resolve remaining audit items C4 and S3
- Resolve 7 review findings (2 critical, 5 high)
- Close 3 remaining high-priority review findings
- Resolve 7 medium-priority review findings
- Resolve 5 low-priority review findings
- Resolve remaining audit items C4 and S3
- Add tests and harden blame/diff/parse edge cases
- Resolve 30 audit findings across security, quality, and architecture
- Merge system messages for MiniMax compatibility, add credential scrubbing and Gemini metadata
- Add allowed_senders to Telegram channel profile, improve dashboard tabs
- Webhook proxy only allocates port for Feishu webhook mode, share reqwest client
- Handle Feishu url_verification challenge at proxy level, return JSON errors
- Return JSON error responses from Feishu webhook handler
- Filter empty assistant messages and show model name in API errors
- Cron consent requirement, name-based removal, and silent response suppression
- Improve cron remove discoverability — LLM now knows to use name-based removal
- Plugin loader permission denied on .main_verified + dedup plugin dirs
- Suppress status indicator for cron/system messages
- CORS allow any origin, add Twilio channel, improve error logging
- Admin token login fails when user_store is None
- /api/my/* endpoints now work with admin token auth
- Use JSON merge patch for profile updates to preserve channels/env_vars
- Process leak prevention, UTF-8 safe truncation, headless Chrome
- Add missing platform-skills/asr/SKILL.md to git
- Weather skill multilingual geocoding support
- Filter platform models to Qwen3 ASR/TTS, weather geocoding tweaks
- Use contains() for [SILENT] cron check instead of starts_with()
- Allow unsafe in SwappableProvider lifetime extension methods
- Add SwappableProvider leak_str approach and SwitchModelTool

### Documentation

- Update README, PRD, architecture, and user manual for Phase 8
- Update README, PRD, architecture, and user manual for Phase 9
- Update all docs for OAuth, email, media, vision, voice, skills, Docker
- Update docs for tool policies, sandbox, compaction, and coalescing
- Update PRD and user manual with new features
- Fix group:memory references, add diff_edit and group:search
- Expand ARCHITECTURE.md with detailed technical design
- Expand skills system section in ARCHITECTURE.md
- Expand plugin system section in ARCHITECTURE.md
- Expand progress reporting section in ARCHITECTURE.md
- Add feature parity analysis vs Moltis
- Update feature parity to reflect completed improvements
- Document hooks system in CLAUDE.md
- Mark hooks system complete in feature parity doc
- Update all docs for browser automation tool
- Update browser tool docs with security hardening details
- Sync docs with codebase after recent feature additions
- Update docs to reflect audit hardening changes
- Update CLAUDE.md with audit hardening details
- Add deep technical audit report (2026-02-18)
- Comprehensive README with installation, channel setup, and dashboard guide
- Add search source registry design for Deep Research

### Features

- Add gateway messaging infrastructure (Phase 1)
- Add Telegram and Discord channel integrations (Phase 3)
- Add web_search and web_fetch tools (Phase 4)
- Add memory store, skills loader, and cron service (Phase 5)
- Add full gateway feature parity (Phases 6-7)
- Add interactive chat, system status, Zhipu provider, and onboard (Phase 8)
- Add ListDir tool, cron expressions, CLI subcommands, built-in skills, config migration (Phase 9)
- Add media handling, vision, voice transcription, skills CLI, Docker, WhatsApp login
- Add OAuth login (octos auth) and email channel (IMAP/SMTP)
- Add streaming responses and context window compaction
- Add full roadmap — pricing, MCP, sandbox, plugins, REST API
- Add tool policies, context compaction, and config hot-reload
- Add hybrid memory search (BM25 + HNSW vector similarity)
- Add provider-specific tool policies
- Add message coalescing, session forking, and Docker sandbox
- Execute tool calls concurrently via join_all
- Add wall-clock timeout, tool output sanitization, DNS SSRF protection, deny(unsafe_code)
- Add provider failover chain and SecretString for API keys
- Add MCP HTTP/SSE transport for remote servers
- Add hook/lifecycle system for agent events
- Add web UI, Prometheus metrics, and message queue modes
- Add browser automation tool via Chrome DevTools Protocol
- Implement 6 audit feature gaps across 5 phases
- Rewrite browser tool with chromiumoxide, add multi-LLM routing and deep research tests
- Add admin dashboard, multi-user profiles, security hardening, and 401 failover
- Add WhatsApp media reception, vision-aware content, Gemini thoughtSignature fix
- Add Feishu/Larksuite channel with webhook mode, web search logging
- Require user permission before taking photos
- Add Twilio channel for SMS/MMS/WhatsApp Business messaging
- Add dashboard multi-user auth with email OTP and user management
- Add base_url support and provider mapping to profiles
- Add webhook proxy for Feishu/Twilio, WebSocket default for Feishu
- Add tool config system, /config command, dashboard overhaul, and multi-provider improvements
- Add skill registry search and replace install-mofa with skill-store
- Add PersonaService for dynamic LLM-generated communication style
- Extract built-in skills and tools to external repos
- Restore system skills as built-in (cron, skill-store, skill-creator)
- Interactive research confirmation + fix /new to clear session
- Deep-search v2 — multi-round search, parallel crawl, reference chasing
- Deep search v1 — synthesize_research, adaptive failover, kimi fixes
- Sub-accounts, account-manager skill, hooks enrichment, admin bot, pipeline
- Add admin token login to dashboard login page
- Admin bot page reuses LlmProviderTab for full LLM setup
- Gateway stability — configurable timeouts, session guards, bus fixes
- Externalize system prompts + add admin_update_profile tool
- Admin bot refactor, cron timezone support, monitoring, dashboard updates
- Sub-account update/start/stop/restart, serve watcher enable/disable transitions
- Add update/start/stop/restart to account-manager skill
- Add sub-account dashboard UI, admin tools, and fix watchdog toggles
- Filter platform skills catalog to ASR+TTS only
- Add clock, weather, ASR app skills, voice architecture, admin tools refactor
- CI/CD workflows, self-updater, admin API enhancements, deploy improvements

### Miscellaneous

- Apply rustfmt formatting across workspace
- Apply cargo fmt formatting
- Add node_modules to .gitignore and remove from tracking
- Apply cargo fmt formatting

### Refactor

- Remove task mode (run/resume/list) and coordinator pattern
- Deduplicate truncation, configurable threshold, ~user expansion
