# Changelog

All notable changes to octos will be documented in this file.
## [Unreleased]

### Bug Fixes

- Add GITHUB_TOKEN auth for private repo release checks
- Pass github_token through API for private repo update checks
- Address security review findings and CI hygiene
- Char-safe truncation for voice transcript in status indicator
- Address review findings from Phase 1 audit
- Address review findings from Phase 3 audit
- Address MEDIUM and LOW review findings from Phase 3
- Install skill binaries to ~/.cargo/bin and fix platform_key for macOS
- Treat provider/model changes as hot-reloadable to prevent gateway restart
- Asr uses correct field name and auto-discovers ominix-api URL
- Deploy script auto-detects ASR/TTS models for ominix-api plist
- Strip <think> tags from streaming output before showing to user
- Register /adaptive and /queue in Telegram command menu
- Persist full tool-call context in session history
- Normalize messages before LLM calls and fix speculative overflow
- Prevent tool_call ID collisions in speculative overflow tasks
- Format tool_call_conversation.rs to pass CI
- Resolve clippy warnings breaking CI
- Stabilize flaky speculative overflow test
- Format r9s.rs to pass CI
- WeComBot reconnect counter reset and send error propagation
- Voice-skill clone endpoint and skill-registry URL
- Remove OMINIX_API_URL fallback from voice-skill, use per-service env vars only
- Prevent duplicate message delivery and same-provider hedging
- Use UpdateId.0 accessor for Telegram dedup cast
- Resolve clippy warnings across workspace to pass CI (-D warnings)
- Session switching, streaming bypass, and status indicator deadlock
- Send_file path validation prevents cross-profile file exfiltration
- Reject unresolvable paths in send_file base_dir check (TOCTOU)

### CI

- Add conditional security sandbox test job

### Documentation

- Align documentation with current codebase (#3)
- Add NLSpec features technical documentation
- Update user guides
- Update README and ARCHITECTURE for NLSpec features
- Add local deployment guide, ci.sh, and local-tenant-deploy.sh
- Add measured sandbox performance benchmarks to security architecture
- Add Docker per-user bind mount model and isolation comparison
- OpenClaw gap analysis and search racing improvement plan

### Features

- Add per-session actor model and voice_synthesize tool
- Attractor spec Phase 1 - caching, thinking, loop detection, typed DOT
- Attractor spec Phase 2 - APIs, observability, pipeline infrastructure
- Attractor spec Phase 3 - advanced features and extensibility
- Add manage_skills tool for in-chat skill install/search/remove
- Add metadata support to message tool and forward Telegram callback queries to agent
- Refactor gateway into modules, fix admin plugin loading, add SKILL.md frontmatter
- Adaptive routing, queue modes, streaming, abort, voice skill
- Per-session actor model, voice skill, pipeline, and API improvements
- Voice cloning with x-vector profiles, API channel, plugin SDK, and multi-crate improvements
- Add r9s.ai as a registered LLM provider
- **discord**: Add message edit/delete ops and dashboard Discord tab
- WeCom Group Robot WebSocket channel and streaming dedup fix
- **dashboard**: Add WeCom Bot messaging tab
- Two-tier deferred tool dispatch with activate_tools meta-tool
- **security**: Per-user workspace isolation, /tmp loophole fix, and CI security tests
- **bus**: Channel improvements and telegram fixes
- CLI improvements, deploy script overhaul, and misc fixes
- Add reactions, embeds, and message dedup to Discord channel
- Change default Docker sandbox image to ubuntu:24.04
- **dashboard**: Add Sandbox tab to profile system settings
- Sandbox management via admin bot + deploy Docker/Colima support
- Composable multi-layer status system with per-user config
- Persist OTP auth sessions to disk across server restarts
- Add Exa neural search as top-priority web search provider
- Inject provider env vars and work_dir into plugin processes
- Inline DOT pipeline generation, model QoS metadata, and prompt fixes
- Plugin loader returns MCP servers, hooks, and prompt fragments from skills
- Per-profile sandbox isolation and skill directory layering

### Refactor

- Layered skills architecture, remove hardcoded TTS/deep-research tools
- Add make_reply helper and verify_sub_account to reduce boilerplate
- Session compaction improvements and architecture doc update
- **voice-skill**: Direct port routing, remove Caddy dependency
- **voice**: Update voice platform skill routing

### Testing

- Comprehensive test coverage (+518 tests) and pre-release smoke script (#2)
- Adaptive routing UX tests and session switching architecture doc

### Merge

- Resolve conflicts with origin/main
- Integrate remote main (WeCom Bot PR #12) with local changes

### Style

- Fix cargo fmt formatting
- Fix cargo fmt formatting
- Cargo fmt

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
