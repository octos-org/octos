# Changelog

All notable changes to octos will be documented in this file.
## [Unreleased]

## [2.1.0-rc.1] - 2026-11-02

### Highlights

- Workspace catalog and `SessionRef` sessions: `session/list` gains `workspace_id`, `cursor` and `limit` with `next_cursor` / `scope` in the result, and `session/open` accepts an opaque `session_ref` (UPCR-2026-060, #2890).
- Compact tool dispatch is the default agent surface, and DeepSeek lanes replay `reasoning_content` on every assistant turn whenever tools are present, so thinking-mode tool loops no longer fail on the second step (#2862, #2865, #2878).
- `deepseek-flash` (DeepSeek V4.1 Flash) is catalogued as the DeepSeek family default with its 1M-token window, 384K output cap and current pricing (#2978).
- MCP connectors: profile-scoped connector management, OAuth authorization with scoped token storage, and read-only connector facts over the API (#2912, #2913, #2917).
- Native office generation: DOCX and XLSX (#2883) and PPTX (#2884) skills, with portable PPTX resources shipped in release bundles (#2923).
- Session file access: chunked workspace file reads (#2886), structured write paths replayed in v2 tool starts (`projection.write_path.v1`, #2895), and file-mutation references restored on hydrate (UPCR-2026-066, #2920).
- Turn durability: server-owned turns keep running across reconnects (#2882), an interrupted turn's dispatched prompt is committed to durable history (UPCR-2026-056, #2849), and cancelled user questions emit a durable `user_question/cancelled` notification (#2873).
- Memory: record-only `session/append_message` writes (UPCR-2026-042, #2730), participant-scoped memory extraction (UPCR-2026-053, #2859), app memory requests over OUP (#2848), and background download of the default embedding model (#2893).
- Cron: profile-scoped create and delete methods (#2891) and serve delivery acknowledged through the session queue (#2888).
- Models: Claude Opus 5.5 with adaptive thinking for Claude 4.6+ (#2493); GLM tool images stay text-only (#2924).
- Runtime: fresh profiles use the Node image (#2927) and report their actual session execution environment (#2928); supervised workers resume and hand off stalled check cycles (#2922); Matrix operators can ignore senders to break bot-to-bot loops (#2875).

This release candidate pairs with OctosCode 0.4.0-rc.1. Release bundles include the
server, sandbox helper and bundled skills for macOS ARM64, Linux x64/ARM64
and Windows x64.

### Security

- Media and upload directories are owner-only, inbound filenames are sanitized, the appservice client no longer follows symlinks, foreign-owned private dirs are refused, the `/tmp` media default is gone, the shared uploads root is created private, and plain `/tmp` is dropped from the `send_file` delivery allowlist (#2800, #2805, #2835, #2846).
- Tenant authorization is enforced for every FRP proxy registration (#2904); setup-script URLs no longer carry the tenant auth token (#2855); dashboard profile actions are bound to the current route and SVG file responses are isolated from the dashboard origin (#2905, #2906).
- Memory access is scoped to the selected session (#2919), session files resolve through the bound profile (#2915), and raw-id session lookups resolve through the open's profile binding (#2797).
- Guard hooks fail closed when the breaker trips (#2844); the skills cargo-build fallback sits behind an operator opt-in (#2861); skill mutations in `manage_skills` are confirmation-gated (#2858).
- Workspace media is opened through trusted directory file descriptors (#2946); profile defaults, cost/usage ledgers and account-manager sandbox config fail closed on corrupt or malformed data (#2954, #2965, #2957); the autonomy monitor/expired sink is budgeted per owning session (#2801).
- Dependency advisories: gix 0.83 and jsonwebtoken 10.4 (#2838), rustls 0.23.46 and the vulnerable rustls pulled in through Serenity (#2879, #2955), pyo3 0.29.3 (#2932), lettre 0.11.22 (#2933), rmcp 2.1 (#2841), plus dashboard, swarm-app, octos-web and PPTX-skill package bumps.

### Fixed

- API channel: queued critical progress events are shed beyond a wire-size budget, the progress flush is bounded against a stalled consumer, the turn drain loop wakes on agent completion with a bounded terminal send, durable turn-task sends run on the offloaded lane, approval/question prompt sends are budgeted, and the executor parks during over-cap frame previews (#2808, #2810, #2812, #2816, #2818).
- Prompt-cache fingerprints are independent of JSON key order in both the LLM and agent layers (#2814, #2847).
- Retry and external-failure handling is hardened, SSE errors and incomplete text streams are surfaced, and turns are marked partial when persistence fails (#2943, #2975, #2976).
- Session sequences stay monotonic across independent writers and rollover (#2908); BM25-only writes survive concurrent recall rebuilds (#2910); record-only append retries work beyond the loaded history window (#2854).
- Windows: directories are classified before opening (#2887) and unsupported environment probes are reported (#2930).
- Peer handles derived from Unicode titles are safe ASCII (#2921); typed command details cover the `bash` and `exec_command` aliases (#2750); deep-search fails on artifact write errors (#2967); malformed Ominix config is preserved (#2952); admin implicit launches stay out of user sticky profiles (#2977).
- `send_file` deliveries keep their transcript order (#2937); cron session scope and validation are aligned across the API surface (#2966).

### Changed

- Agent shared DTOs moved to `octos-core` and FFI support moved out of the CLI (#2944, #2947); embedding and session memory lifecycle is bounded (#2950); immutable `ToolSpec` artifacts are cached across turns (#2988).
- CI: UPCR coverage gate in the check job (#2794), release gating on npm/Homebrew distribution alignment (#2799), branch hygiene policy and quality ratchet (#2804), a `test-octos-cli-gate` so required checks survive research-only skips (#2843), affected-test runs on ordinary PRs (#2963), required checks for merge groups (#2958), macOS jobs on macos-15 (#2925), Windows integration libtest de-duplication (#2972), and fail-closed handling of unmapped build configs (#2974).
- Test-suite and docs housekeeping (table-driven recovery scenarios, Windows lane de-noising, approval/recovery invariants, architecture and security doc corrections) and routine dependency bumps for the dashboard, swarm-app, octos-web and the PPTX skill's Python packages.

## [2.0.3] - 2026-10-13

### Highlights

- Accept new instructions during an active turn: wake waiting model requests and reconsider tools that have not started yet.
- Coordinate independently opened sessions in the same workspace through the shared kernel and peer messaging.
- Use native research with shared provider credentials instead of the retired DOT research pipeline.
- Preserve long-session context with incremental checkpoints and use the session's selected model for both automatic and manual compaction.
- Append ordinary model-loop context changes instead of rewriting the full checkpoint before each model request; rotate transition journals at an 8 MiB byte budget (#2793).
- Include provider, skill discovery, session recovery, host-owned app isolation and OUP compatibility fixes from the release candidates and subsequent main-branch changes.

This stable release pairs with OctosCode 0.3.1. Release bundles include the
server, sandbox helper and bundled skills for macOS ARM64, Linux x64/ARM64
and Windows x64.

### Security

- Enforce connection ownership when registering, unregistering or replacing peer and session tool routes, preventing another connection from taking over a live route (#2690).
- Apply the configured API-channel authentication token across chat, event streams and the other API routes; remove token-length and local-path disclosures from admin-shell authentication failures (#2735, #2752, #2753).
- Consolidate constant-time credential comparisons and use them for Feishu tokens and Twilio/WeCom signatures (#2752, #2753, #2754).
- Reject untrusted Host headers and cross-site Origins on the API channel, and reject duplicate Host headers on host-managed servers (#2755, #2759).
- Reject credential-less Feishu events when webhook authentication is configured, treat empty encryption keys as unconfigured, and enforce signature timestamp freshness with a matching deduplication window (#2760, #2774, #2775).
- Bind Matrix appservices to loopback by default, restrict generated registration files to their owner on Unix, and deprecate query-string token authentication in favor of bearer headers (#2733).
- Update h2, tar and rand to versions addressing the dependency advisories tracked in #2744.

### Fixed

- Reclaim stale workspace-team members at the team cap while preserving live connections and active turns, and fence coordinator changes when an old leader is evicted (#2766).
- Retry transient memory-refresh lock setup failures so a temporary filesystem error does not disable refresh for the runtime's lifetime (#2767).

### Features

- Smart Home control — list and control smart-home devices (lights, thermostats, curtains, etc.) via a per-profile bridge (e.g. Home Assistant), through both the UI Protocol (`smart_home/*` WS methods, backing octos-web's Smart Home panel) and a new bundled `smart-home` agent skill (`smart_home_list_devices`, `smart_home_control_device`). Camera video streaming stays a human-facing, WebSocket-only feature and is not exposed to the agent.

### Changed

- Z.AI: the `zai` and `zai-coding` lanes (and the `octos init` Z.AI preset) now speak OpenAI Chat Completions at `https://api.z.ai/api/paas/v4` / `https://api.z.ai/api/coding/paas/v4`, where Z.AI's implicit prompt cache is reported and billed; the Anthropic-compatible root never honoured cache breakpoints. Saved routes naming `https://api.z.ai/api/anthropic` are migrated with a warning; `api_type: "anthropic"` keeps the old protocol (without a `base_url` it targets the Anthropic-compatible root).
- Capped tool outputs and tier-1 pruning placeholders now name what was cut (target, shown/original size) and how to get it back (`recall` by tool_call_id, or a narrower re-run).
- New opt-in UI protocol feature `context.state.v1` / `context/state_reported`: a live context-fullness gauge during long turns, delivered only to connections that negotiated it.
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
