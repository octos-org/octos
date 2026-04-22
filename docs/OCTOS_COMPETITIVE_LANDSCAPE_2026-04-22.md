# Octos Competitive Landscape — 2026-04-22

See also:

- [OCTOS_PROTOCOL_STRATEGY_2026-04-22.md](./OCTOS_PROTOCOL_STRATEGY_2026-04-22.md)
- [OCTOS_SWARM_POSITIONING_2026-04-22.md](./OCTOS_SWARM_POSITIONING_2026-04-22.md)

## Purpose

Capture a structured comparison of octos against the two codebases the team has asked about as competitive references:

- `hermes-agent` — Python agent framework, coding-loop specialist
- `openJiuwen-ai` — multi-project Chinese agent platform with web Studio + A2A C++ SDK + RL training

The comparison is deliberately coding-harness-centric. Other axes (multi-channel, platform surface, enterprise features) are noted where they differ materially but are not the primary lens.

## Verdict

The three systems occupy distinct vertices, not a shared axis:

- **hermes** — coding-loop depth specialist; Claude-Code-class CLI; ACP bidirectional; ~19 kLOC coding-specialized Python
- **octos** — contract-first API platform; typed workspace policy; executable skill binaries; 14-channel bus; Rust type-safety; weak CLI UI; no ACP
- **openJiuwen** — web-Studio-first platform; Chinese enterprise IM heavy; agent teams / swarm primitives; actual RL training; A2A C++ SDK; MCP-only protocol stack (ACP stub); deep 3-tier LLM compression

For free-form 60-turn coding sessions with API instability, **hermes leads**. For contract-first background-task delivery with multi-tenant safety, **octos leads**. For enterprise agent deployment with dashboard + Chinese IM + team orchestration, **openJiuwen leads**. None of the three dominates.

## Hermes capability profile

### Footprint
~19 kLOC of coding-specialized Python:

- `run_agent.py` — 10,492 LOC. 7 distinct retry-counter classes (`_invalid_tool_retries`, `_invalid_json_retries`, `_empty_content_retries`, `_incomplete_scratchpad_retries`, `_codex_incomplete_retries`, `_thinking_prefill_retries`, `_unicode_sanitization_passes`). Budget-grace-call gives one free iteration past the hard budget cap.
- `agent/context_compressor.py` — 777 LOC. LLM-based iterative summarization with a structured Goal / Constraints / Progress / Decisions / Files / Next Steps template. On subsequent compactions, *updates* the previous summary rather than summarizes from scratch.
- `agent/error_classifier.py` — 809 LOC. 15-reason enum (`auth`, `auth_permanent`, `billing`, `rate_limit`, `overloaded`, `server_error`, `timeout`, `context_overflow`, `payload_too_large`, `model_not_found`, `format_error`, `thinking_signature`, `long_context_tier`, `format_error`, `unknown`) with 4 recovery-hint flags (`retryable`, `should_compress`, `should_rotate_credential`, `should_fallback`).
- `agent/credential_pool.py` — 1,319 LOC. Persistent per-credential cooldowns, OAuth refresh, 4 rotation strategies (`fill_first`, `round_robin`, `random`, `least_used`), provider-supplied `reset_at` timestamps, JWT claims decoding, disk-persisted.
- `agent/smart_model_routing.py` — 195 LOC. Content-classified cheap-vs-strong routing.
- `tools/delegate_tool.py` — 1,088 LOC. Synchronous subagent with MAX_DEPTH=2 safety cap.
- `hermes_state.py` — 1,238 LOC. SQLite+FTS5, schema v6, WAL, parent_session chains.
- `mini_swe_runner.py` — 709 LOC. SWE-bench-oriented separate runner.
- `acp_adapter/` — 1,784 LOC bidirectional ACP server.
- `agent/copilot_acp_client.py` — 570 LOC ACP client wrapping Copilot.
- `agent/retry_utils.py` — jittered exponential backoff with monotonic counter seed for decorrelation.

### CLI
Explicitly modeled on Claude Code. `pyproject.toml` pins `prompt_toolkit>=3.0.52` + `rich>=14.3.3`. Uses curses for interactive checklists. Custom `KawaiiSpinner`, diff renderer, fenced `<memory-context>` markers to prevent memory/discourse confusion.

### Protocol
ACP bidirectional. Registered as ACP agent. Can serve editors (Zed, etc.) and can consume Copilot's ACP server as a chat backend.

### Weaknesses
Not an API platform. Skills are prose markdown (advisory, not executable). Thin channel gateway. Single provider-per-request. No workspace-contract or validator runner.

## Octos capability profile

### Footprint (post-M4 merges, current main)
- `octos-agent` — agent loop, tool system, sandbox, MCP, compaction, plugins
- `octos-bus` — 14-channel message bus (Telegram/Discord/Slack/WhatsApp/Email/WeChat/Matrix/etc.), sessions, coalescing, cron, heartbeat
- `octos-pipeline` — DOT-graph pipeline engine with per-node model selection, parallel fan-out, deadlines, checkpoints, human gates
- `octos-plugin` — plugin SDK with manifest discovery, gating, `hardware_lifecycle` (RP02)
- `octos-llm` — 4 native providers + 8 OpenAI-compatible; 3-layer failover (`RetryProvider` → `ProviderChain` → `AdaptiveRouter` with hedge racing + circuit breakers)
- `octos-memory` — redb EpisodeStore + MEMORY.md + HybridSearch (BM25 + HNSW vector)
- `octos-core` — Task, Message, Error types
- `octos-cli` — CLI + 91 REST endpoints via `octos serve`

### Landed M4 surface
- `harness_events.rs` — `octos.harness.event.v1` schema + `OCTOS_EVENT_SINK` env protocol (M4.1A.470+471)
- `validators.rs` — declarative validator runner (command / tool / file-exists) with evidence ledger (M4.3)
- `abi_schema.rs` — `schema_version` on WorkspacePolicy, HookPayload, TaskResult, ProgressEventEnvelope (M4.6)
- `hooks.rs` — 10 lifecycle events with HookPayloadEnricher + domain_data (M4.1A + RP03)
- `TaskLifecycleState` — stable public states (`Queued`/`Running`/`Verifying`/`Ready`/`Failed`) (M4.1A)
- Operator dashboard React pages (M4.5)
- 4 starter skills (report, audio, coding, generic) (M4.2)
- RP family: SafetyTier as ToolPolicy groups, sandboxed HardwareLifecycle, domain-hook pattern, pipeline deadline/checkpoint, realtime heartbeat + sensor injection, dora-mcp removed

### CLI
`colored = "2"` + `clap` only. No `ratatui`/`crossterm`/`indicatif`/`dialoguer`. Functional not polished. The presentation investment went into the admin dashboard.

### Protocol
MCP client only. No ACP (neither client nor server). No A2A.

### Unique strengths
- Rust type-safety + `deny(unsafe_code)` workspace-wide
- Typed durable workspace contracts
- Hybrid BM25+HNSW memory search
- 3-backend local sandbox (Bwrap / macOS sandbox-exec / Docker) with shared `BLOCKED_ENV_VARS`
- Symlink-safe `O_NOFOLLOW` file I/O
- Shared SSRF protection (`tools/ssrf.rs`)
- Cross-platform (Windows `cmd /C`, Unix `sh -c`)
- Rustls TLS (no OpenSSL)
- Plugin system with executable skill binaries

### Weaknesses
Coding-loop resilience thin. No credential pool, no persistent per-key cooldowns, no structured error taxonomy, no LLM-based iterative compression. CLI presentation minimal. No ACP. No multi-agent swarm primitive.

## OpenJiuwen capability profile

### Footprint
538 MB across 6 subprojects; ~470 kLOC Python + TypeScript + C++:

- `agent-core/` (~189 kLOC Python) — SDK + `harness/` DeepAgent coding framework (`deep_agent.py` 1,533 LOC, `factory.py` 314 LOC, `task_loop/` 3,546 LOC, `rails/` 3,588 LOC, `tools/` 4,560 LOC, `subagents/` browser + code + research)
- `agent-studio/` (~172 kLOC: 54 kLOC FastAPI backend + 116 kLOC React/TS frontend + `plugin_server/` + `sandbox_server/`) — full agent-creation IDE, 22 routers, helm charts
- `agent-protocol/` (~30 kLOC C++) — full A2A C++ SDK + MCP C++ SDK
- `agent-tools/` — vLLM affinity + AIC reference agents
- `deepsearch/` (48 kLOC Python) — separate deep-research service
- `jiuwenclaw/` (82 kLOC Python + 20 kLOC TS) — the end-user coding/chat app; 14-channel bus with Chinese enterprise IM depth (Feishu 1,827 LOC, DingDing 1,122, WeCom 914, WeChat 1,032, Xiaoyi 1,458); `agentserver/` (7,943 LOC); `deep_agent/permissions/` (1,098 LOC) with LLM-assessed command risk; `skilldev/` 10-stage pipeline (3,526 LOC)

### Coding-loop depth
`react_agent.py` invoke loop. Single `for iteration in range(max_iterations)` (15 default, `sys.maxsize` when `enable_task_loop=True`). One-shot context-fix retry. **No retry-counter classes. No credential pool. No semantic error classifier. No MAX_DEPTH on subagent.**

### Context management
3-tier LLM-based compressor:
- `DialogueCompressor` (551 LOC) — ReAct-block JSON compression with priority ordering and bilingual preservation
- `RoundLevelCompressor` (1,150 LOC) — aggressive fallback
- `CurrentRoundCompressor` (1,006 LOC) — in-flight round compression

Strong design; **comparable to hermes's iterative template** though simpler in that it's single-pass rather than iterative-refinement-of-prior-summary.

### Protocol
Full C++ SDKs for **A2A** (Google's Agent-to-Agent) and **MCP**. A2A is a unique capability neither hermes nor octos has. ACP exists only as a 79-LOC stub channel in `jiuwenclaw/channel/acp_channel.py`.

### RL training
`dev_tools/agentrl/` (5,563 LOC) with PPO trainer, rollout store, `verl_executor.py` for VeRL framework integration, reward registry. `jiuwenclaw/evolution/` (1,909 LOC) with signal_detector, evolver, online experience generation. **Genuine RL infrastructure** — hermes and octos have none.

### Web Studio
~170 kLOC full-stack React + FastAPI agent-creation IDE. Pages for Agents, Workflows, Models, Prompts, KnowledgeBase, MemoryBase, Plugins, Dashboard. Plus `plugin_server/` and `sandbox_server/` (FastAPI gateway + Python kernel multi-process isolation + JS kernel + Dockerfiles). This is the **supervisor UI** octos lacks.

### Weaknesses
Not safe under API instability (no credential pool, no structured error recovery). Thin CLI. No ACP. No SWE-bench runner. Permissions system uses LLM-assessed risk (`assess_command_risk_with_llm`) which adds latency and has non-determinism concerns.

## Capability matrix

| Axis | hermes | octos | openJiuwen |
|------|--------|-------|------------|
| Coding-loop retry counters | 7 classes | None structured | None |
| LLM failover / credential pool | Persistent per-credential cooldowns, 4 strategies, OAuth refresh | 3-layer (Retry → Chain → Adaptive) | None |
| Structured error taxonomy | 15-reason enum + 4 recovery flags | None | None |
| LLM-based context compaction | Iterative, structured Goal/Progress template | Extractive only | 3-tier single-pass |
| Sandbox | Local subprocess only | 3-backend local (bwrap / macOS / Docker) | Remote containerized gateway |
| Workspace contract | None | Typed + durable | None |
| Validator runner | None | Declarative (M4.3) | Generic ReAct rail completion |
| Executable skill binaries | Prose markdown only | Yes, manifest + JSON protocol | Skill marketplace + 10-stage skilldev pipeline |
| Subagent / delegation | Synchronous, MAX_DEPTH=2 | `spawn_only` background | Synchronous TaskTool, **no depth limit** |
| Agent teams / swarm | None | None | TeamAgent system (8,882 LOC) |
| RL training | None | None | agentrl (5,563 LOC) + online evolver |
| SWE-bench runner | `mini_swe_runner.py` 709 LOC | None | None |
| CLI UI framework | prompt_toolkit + rich + curses | `colored` + `clap` only | Stub (54 LOC `app_cli.py`) |
| Web Studio / supervisor UI | Minimal | Admin dashboard (M4.5) | Full-stack Studio (~170 kLOC) |
| Multi-channel bus | 1 channel | 14 channels | 14 channels (Chinese IM heavy) |
| ACP server | ✓ (1,784 LOC) | ✗ | Stub (79 LOC) |
| ACP client | ✓ (Copilot shim 570 LOC) | ✗ | ✗ |
| MCP client | ✓ | ✓ | ✓ (Python) + C++ SDK |
| MCP server (agent-as-tool) | ✗ | ✗ | ✗ |
| A2A (Google agent-to-agent) | ✗ | ✗ | ✓ (C++ SDK) |
| Hybrid BM25 + HNSW memory | ✗ | ✓ | Graph + lite + long-term split |
| Durable task lifecycle states | Session-level | `TaskLifecycleState` enum | Generic session persistence |
| Schema-versioned ABI | ✗ | ✓ (M4.6) | ✗ |
| Deny(unsafe_code) type safety | Python | Rust | Python |

## Coding-loop failure mode analysis

For a 60-turn coding session on flaky LLM infrastructure:

- **hermes**: survives. Error classifier routes 429 → credential rotation or wait. Retry counters catch and recover malformed tool calls. Iterative compressor keeps architectural decisions from turn 15 available at turn 45. Budget-grace-call lets the model summarize at end.
- **octos**: survives provider-level outages (3-layer failover) but loses session coherence on long runs — extractive compaction doesn't preserve architectural decisions; no retry-counter taxonomy for tool-call malformation.
- **openJiuwen**: drops at turn 30-45 if the provider hiccups. No credential pool. One-shot retry only. The 3-tier LLM compressor keeps it going on coherence, but compression itself consumes tokens without adaptive routing to cheap models for the compression call.

## Delta estimates

### To reach hermes parity on free-form coding

| Target | From octos | From openJiuwen |
|---|---|---|
| Credential pool | ~1,300 LOC Rust | ~1,300 LOC Python |
| Structured error classifier | ~800 LOC | ~800 LOC |
| Smart content-classified routing | ~200 LOC | ~200 LOC (already has AdaptiveRouter pattern implicit) |
| ACP bidirectional | ~3,500 LOC | ~2,000 LOC (stub to replace) |
| SWE-bench runner | ~1,000 LOC | ~1,000 LOC |
| MAX_DEPTH on subagent | ~100 LOC | ~100 LOC |
| Iterative context-refinement-of-prior-summary | ~500 LOC | ~300 LOC (already has 3-tier base) |
| CLI TUI (ratatui / prompt_toolkit equivalent) | ~2,000 LOC | ~2,000 LOC |
| **Total** | **~9,400 LOC Rust** | **~7,700 LOC Python** |
| **Time** | 3-4 engineer-months | 2-3 engineer-months |

### To reach openJiuwen parity on platform surface (octos)

Much larger gap than the hermes gap:
- Web Studio agent-creation IDE: ~100-120 kLOC React + 40-50 kLOC Rust backend; **6-12 months, 3 engineers**
- Agent teams / swarm: ~9 kLOC Rust; 2-3 months
- RL training: ~5 kLOC (leveraging existing RL framework); 3-6 months
- Skill marketplace + 10-stage skilldev pipeline: ~4 kLOC; 2 months
- A2A protocol: ~3-5 kLOC Rust port; 1-2 months
- Chinese enterprise IM channels (Feishu/DingDing/WeCom/WeChat/Xiaoyi): ~6 kLOC Rust; 3-4 months
- **Total: 120-170 kLOC over 12-18 months, 4-5 engineer team**

The openJiuwen gap is an order of magnitude wider than the hermes gap. Closing it would be a different company, not a different milestone.

## Strategic implication

Octos's current roadmap (M4 productization → M5 coding runner contract) pursues **contract-first platform quality** — the same axis where octos already differentiates. This is defensible and focused.

Pursuing hermes parity would require adding the 9.4 kLOC of coding-loop-resilience work, most of which sits outside the current M4/M5 contract vocabulary (error taxonomy, credential pool, iterative compression are loop concerns, not contract concerns). That work should either go into a dedicated **M6 coding loop hardening** milestone or be explicitly deferred.

Pursuing openJiuwen parity would require a pivot — web Studio + agent teams + RL training represent a different product bet. Not recommended unless the company decides to compete with openJiuwen on Chinese enterprise agent deployment, which is a sales-first question not a roadmap-first question.

## Recommended posture

- **Keep octos's axis**: contract-first API platform. M5 coding runner contract is consistent.
- **Close the two highest-leverage protocol gaps** (see `OCTOS_PROTOCOL_STRATEGY_2026-04-22.md`): MCP-server-of-octos so orchestrators can call octos as a sub-agent; MCP-client-to-other-agents (Claude Code, Codex) so octos can orchestrate them.
- **Defer hermes-style coding-loop depth** unless user signal demands it. The contract-first completion gate (M5) is a different approach to solving "how do we trust a coding agent": hermes answers via loop resilience, octos answers via contract-gated delivery. Both are valid; they're different products.
- **Do not pursue openJiuwen-class Web Studio** unless the enterprise-agent-deployment market becomes the primary product. Current roadmap implies it is not.

## Next review

Revisit this analysis after M5 completion. If an M6 coding-loop milestone is opened, this doc should be the gap spec for it.
