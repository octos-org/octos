# Octos Coding Hardening — M6 Plan (Track 1)

See also:

- [OCTOS_COMPETITIVE_LANDSCAPE_2026-04-22.md](./OCTOS_COMPETITIVE_LANDSCAPE_2026-04-22.md)
- [OCTOS_PROTOCOL_STRATEGY_2026-04-22.md](./OCTOS_PROTOCOL_STRATEGY_2026-04-22.md)
- [OCTOS_SWARM_POSITIONING_2026-04-22.md](./OCTOS_SWARM_POSITIONING_2026-04-22.md)

## Purpose

Close the free-form coding capability gap with hermes, so octos stands as a strong standalone coding agent. Do this by **re-expressing every hermes innovation through octos's native harness primitives** — not by porting hermes's code.

This is **Track 1**. Track 2 (PM + agent swarm orchestrator — MCP sub-agent dispatch, Matrix-as-supervisor-UI, remote-agent dispatch) is deliberately out of scope here and will open as a separate family after M6 lands.

## Why "not copy hermes"

Hermes earned its capability through ~19 kLOC of Python accumulated in a coding-specialist context. The innovations are real — 15-reason error taxonomy, iterative LLM compression, persistent credential pool, 7 retry counter classes, smart content routing — but the implementation is structured for a Python REPL-first single-agent CLI. Direct port to Rust would give octos hermes's capability, but miss **the leverage of octos's harness foundation**.

Octos already has:

- M4.1A **structured progress events** (`octos.harness.event.v1`) over `OCTOS_EVENT_SINK` → every harness-level signal is observable and replayable
- M4.3 **declarative validator runner** → completion is evidence-based, not prose-based
- M4.6 **schema-versioned ABI** → `WorkspacePolicy` / `HookPayload` / `TaskResult` / `ProgressEventEnvelope` all carry `schema_version`; external developers can depend on stable shapes
- M4.5 **operator dashboard** → runtime truth is visible without log archaeology
- M5 **coding runner contract** (phase classifier + evidence-based completion gate)
- `RetryProvider` / `ProviderChain` / `AdaptiveRouter` — existing 3-layer LLM failover
- Rust type-safety + `deny(unsafe_code)` — compile-time guarantees hermes cannot offer
- Hybrid BM25+HNSW memory
- 3-backend sandbox with shared `BLOCKED_ENV_VARS`, symlink-safe `O_NOFOLLOW`, shared SSRF protection
- Executable skill binaries (not prose markdown)

**The re-expression principle**: each hermes innovation becomes typed, schema-versioned, event-observable, validator-gateable, and session-durable on octos. The feature composes with the rest of the harness rather than sitting alongside it as an isolated coding-loop hack.

## How re-expression wins per feature

| Hermes feature | Hermes shape | Octos M6 shape (built on harness) |
|---|---|---|
| 15-reason error classifier | 809 LOC Python with hardcoded regex patterns, 4 recovery-hint flags | **Typed `HarnessError` enum**, schema-versioned, emitted as `harness.event.v1 error` records via `OCTOS_EVENT_SINK`. Dashboard shows per-variant rates. Validator can gate on "no unrecovered rate-limit in last N turns." Event replay reconstructs loop health post-hoc. |
| Persistent credential pool | 1,319 LOC Python with file-based JSON, 4 strategies, OAuth refresh | **`CredentialPool` trait + redb-backed state**, typed cooldown expiry, schema-versioned config, rotation events emitted to sink. Integrates with existing `AdaptiveRouter`. Config lives in profile-level typed section (M4.6 schema-stable). |
| Iterative context compression with Goal/Progress/Decisions template | 777 LOC Python with prompt engineering | **Typed `SessionSummary { goal, constraints, progress_done, progress_in_progress, decisions, files, next_steps }`** with serde round-trip. Compaction emits progress events per phase (analyzing → summarizing → refining → done). Next-summary-updates-prior-summary pattern in durable state. Validator gates: "summary must preserve declared artifacts." |
| 7 per-iteration retry counters | `self._invalid_tool_retries`, `self._invalid_json_retries`, etc. sprinkled through 10,492-LOC `run_agent.py` | **`LoopRetryState` struct** with typed variants, bounded per-variant limits, state survives compaction, each bucket emits typed retry events. Operator dashboard shows retry-rate per kind. Budget controller (M6.2) can decide "escalate this session — 3/3 invalid-tool retries exhausted." |
| Budget-grace-call (one free iteration past hard cap) | Ad-hoc flag in loop body | **Typed `BudgetState::GracePending`** event. Validator can gate "grace call must include summarization." Observable on dashboard. |
| Content-classified smart model routing | 195 LOC Python keyword heuristic | Extend existing `AdaptiveRouter` with **schema-versioned content classifier config**. Classifier is pure (no LLM call), decision emitted as event. Operator can A/B-test routing policies via schema-stable profile config. |
| Delegate-task subagent with MAX_DEPTH=2 | 1,088 LOC Python ThreadPoolExecutor, blocked-tools list | **`DelegateTool`** synchronous sibling of `SpawnTool` with typed `DepthBudget`, MAX_DEPTH via config, child task emits contract-gated artifacts. Reuses M4.1A event sink, M4.3 validator runner, M4.6 ABI. Restricted toolset declared as policy groups (RP01 style). |
| Preflight compression | Conditional block before first API call | **Compaction policy** (M6.3) declares preflight budget; harness event emitted when triggered. Validator gates: "preflight must not drop declared artifacts." |
| Tool output pruning | Cheap pre-pass replacing old tool results | Expressed as **pre-compaction validator rule**: "tool results older than N turns → replaced with placeholder preserving {tool_name, turn_id}." Schema-versioned placeholder format so downstream consumers don't break. |
| Fenced `<memory-context>` tags | String-level XML wrapper | Typed wrapping in `SessionSummary` serde; memory context becomes a first-class field with schema version. Rendering is one option, not the truth. |

Every row shows the same pattern: **hermes has the implementation; octos-M6 has the implementation AND the platform primitive**.

## M6 sub-milestone structure

Six contract-bound issues following the RP / M4 family style.

### M6.1 — Structured harness error taxonomy

**Problem**: octos's errors are `eyre::Result<T>` with no runtime structure. Failure modes can't be observed, counted, or gated.

**Deliverables**:
- New `crates/octos-agent/src/harness_errors.rs` with typed `HarnessError` enum (variants: `ProviderRateLimit`, `ProviderAuth`, `ProviderOverloaded`, `ProviderServerError`, `ProviderTimeout`, `ProviderPayloadTooLarge`, `ProviderModelNotFound`, `ProviderFormatError`, `ThinkingSignatureError`, `ContextOverflow`, `ToolCallMalformed`, `ToolResultInvalid`, `EmptyContent`, `Incomplete`, `UnknownTransient`, `UnknownTerminal`)
- Each variant carries `RecoveryHint::{Retry, RotateCredential, Compact, Fallback, FailFast, Grace}`
- `HarnessError` implements `From<octos_llm::LlmError>` with pattern-based classification
- Emits `harness.event.v1 { kind: "error", variant, recovery_hint }` to `OCTOS_EVENT_SINK`
- Prometheus: `octos_loop_error_total{variant, recovery}` counter
- M4.5 dashboard card: per-variant error rate per session
- Schema-versioned per M4.6 (`HARNESS_ERROR_SCHEMA_VERSION = 1`)

**Size**: ~800 LOC Rust incl. tests.

**Dependencies**: M4.1A, M4.6.

### M6.2 — Loop retry-bucket state machine

**Problem**: Single counter in `loop_runner.rs` can't distinguish "provider rate-limited 3 times" from "model returned malformed JSON 3 times" — different recoveries needed.

**Deliverables**:
- New `crates/octos-agent/src/agent/loop_state.rs` with typed `LoopRetryState`
- Per-variant bounded counters matching M6.1's `HarnessError` variants
- `LoopRetryState::observe(HarnessError) -> LoopDecision` returns `{Continue, Escalate, Exhausted, RotateAndRetry, CompactAndRetry}`
- State serde-stable (survives compaction + session reload per existing session persistence)
- Integrates with existing `recover_shell_retry_output` (which becomes a `LoopRetryState::ShellSpiral` variant)
- **Budget-grace-call**: `LoopRetryState` understands "one free iteration past budget if at least one productive tool call happened"
- Events emitted per retry observation

**Size**: ~500 LOC Rust.

**Dependencies**: M6.1.

### M6.3 — Contract-gated compaction policy

**Problem**: Octos compaction (`crates/octos-agent/src/compaction.rs`, extractive only) doesn't preserve declared invariants. On long coding sessions, architectural decisions at turn 15 vanish by turn 45.

**Deliverables**:
- `WorkspacePolicy.compaction` — declarative compaction policy (M4.3 validator-style)
- Fields: `token_budget`, `preserved_artifacts` (glob list), `preserved_invariants` (goal, decisions, file_list), `summarizer` (extractive | llm-iterative), `preflight_threshold`, `prune_tool_results_after_turns`
- New `Summarizer` trait in `crates/octos-agent/src/summarizer.rs` with impls
- Compaction itself emits progress events (`compaction.phase = "analyzing" | "summarizing" | "refining" | "done"`)
- Validator rail: "post-compaction, declared artifacts must still be referenced in context"
- Preflight compaction: before first LLM call, check token budget vs declared threshold
- Tool output pruning: typed `ToolResultPlaceholder { tool_name, turn_id, schema_version }` replaces old results

**Size**: ~1,200 LOC Rust (policy parsing + trait + extractive impl + preflight + pruning).

**Dependencies**: M4.3 validator runner.

### M6.4 — LLM-iterative summarizer

**Problem**: Extractive compaction preserves recency but loses semantic coherence. For coding sessions where turn-15 decisions drive turn-45 edits, an LLM must do the work.

**Deliverables**:
- `LlmIterativeSummarizer` impl of `Summarizer` trait
- Typed `SessionSummary` struct — serde-stable, schema-versioned
  ```rust
  struct SessionSummary {
      schema_version: u32,  // = 1
      goal: String,
      constraints: Vec<String>,
      progress_done: Vec<String>,
      progress_in_progress: Vec<String>,
      decisions: Vec<DecisionRecord>,
      files: Vec<FileRecord>,
      next_steps: Vec<String>,
  }
  ```
- Structured prompt template (not free prose — returns JSON matching `SessionSummary`)
- Iterative refinement: next summary takes prior summary + new turns → outputs updated summary (not from-scratch)
- Fallback to extractive if LLM call fails 3x
- Schema-versioned per M4.6; compat test with legacy session state

**Size**: ~900 LOC Rust.

**Dependencies**: M6.3, M4.6.

### M6.5 — Credential pool + failover

**Problem**: A single 429 from Claude API at turn 47 kills a 60-turn coding session. `AdaptiveRouter` does hedge racing across providers but doesn't persist per-credential state or rotate within a provider.

**Deliverables**:
- New `crates/octos-llm/src/credential_pool.rs`
- `CredentialPool` trait with 4 strategies: `FillFirst`, `RoundRobin`, `Random`, `LeastUsed`
- Redb-backed persistent state: per-credential cooldown expiry, 429 count, `reset_at` from response headers, last-used timestamp
- OAuth refresh hook for credentials that support it
- Integrates with existing `AdaptiveRouter` → rotation events emitted via `OCTOS_EVENT_SINK`
- Typed `CredentialPoolConfig` in profile config (M4.6 schema-versioned)
- Strategy selection is config-driven, not hardcoded
- Counter `octos_llm_credential_rotation_total{reason, strategy}`

**Size**: ~1,400 LOC Rust.

**Dependencies**: M4.6. Can start in parallel with M6.1/M6.2.

### M6.6 — Content-classified smart routing

**Problem**: Today octos's `AdaptiveRouter` routes by provider health, not by content complexity. Simple turns (e.g., "yes, continue") consume expensive model budget.

**Deliverables**:
- Extends `AdaptiveRouter` with `ContentClassifier`
- Pure heuristic classifier: message length, code-fence presence, URL presence, keyword matching (`debug`, `refactor`, `fix`, `test`, `trace`, `error`, `implement` → strong model; otherwise → cheap model)
- Typed `RoutingConfig` in profile config with overridable keyword list
- Routing decision emitted as event
- A/B toggle: route all / route none / route by classifier (operator can disable for sensitive deployments)

**Size**: ~400 LOC Rust.

**Dependencies**: M6.5.

### M6.7 — Synchronous delegate tool

**Problem**: Octos has async `SpawnTool` (fire-and-forget, deliver-later). No synchronous "parent blocks until child done" pattern. Forces unnatural workflow decomposition.

**Deliverables**:
- New `crates/octos-agent/src/tools/delegate.rs`
- `DelegateTool` is synchronous sibling of `SpawnTool`
- Typed `DepthBudget` with configurable `MAX_DEPTH = 2` (grandchild rejected by default)
- Child task inherits parent's workspace contract but runs with restricted tool policy (declared via RP01-style groups: `group:delegated` excludes `delegate_task`, `clarify`, `memory`, `send_message`, `execute_code`)
- Result flows back through contract-gated delivery (M4.1A-compatible)
- Parent waits on child via existing `TaskSupervisor` lifecycle; integrates with `TaskLifecycleState`

**Size**: ~700 LOC Rust.

**Dependencies**: M4.1A, RP01 (ToolPolicy groups).

### M6.8 — Operator dashboard integration

**Problem**: M6.1-M6.7 add many new events, counters, and state — M4.5 dashboard doesn't show them by default.

**Deliverables**:
- Extend `HarnessPage.tsx` with coding-loop health cards:
  - Error taxonomy breakdown (per session and aggregate)
  - Retry bucket state (which sessions spiraling)
  - Compaction event log with semantic context
  - Credential pool status (cooldowns, rotations)
  - Routing decisions breakdown (cheap vs strong)
  - Delegate tree visualization (per session)
- Backend surfaces via existing admin API; no new API shape

**Size**: ~1,500 LOC TypeScript + ~300 LOC Rust.

**Dependencies**: M6.1 through M6.7.

## Phasing and parallel dispatch

```
Phase M6A (foundations, parallel)
┌──────────────────────────────────┐
│ M6.1 Structured error taxonomy   │  ← 2 weeks, independent
│ M6.5 Credential pool + failover  │  ← 2-3 weeks, independent (depends on M4.6 only)
└──────────────────────────────────┘
              │
              ▼
Phase M6B (loop hardening, parallel)
┌──────────────────────────────────┐
│ M6.2 Loop retry-bucket state     │  ← 2 weeks, depends on M6.1
│ M6.6 Smart routing               │  ← 1 week, depends on M6.5
│ M6.7 Delegate tool               │  ← 2 weeks, depends on RP01 (done) + M4.1A
└──────────────────────────────────┘
              │
              ▼
Phase M6C (context management, sequential)
┌──────────────────────────────────┐
│ M6.3 Compaction policy           │  ← 2-3 weeks, depends on M4.3
│ M6.4 LLM-iterative summarizer    │  ← 3 weeks, depends on M6.3
└──────────────────────────────────┘
              │
              ▼
Phase M6D (surfaces)
┌──────────────────────────────────┐
│ M6.8 Dashboard integration       │  ← 2 weeks, depends on M6.1-M6.7
└──────────────────────────────────┘
```

Total: **~7,700 LOC Rust + 1,500 LOC TypeScript**, **3-4 engineer-months** with parallel dispatch of M6A (2 engineers), M6B (2-3 engineers, 1 per workstream), M6C (1 engineer, sequential), M6D (1 engineer + frontend support).

## Success criteria

M6 is complete when all of the following hold:

1. **60-turn coding session** on a flaky LLM provider completes successfully. Criterion: zero session losses across 10 runs with simulated 429 storms.
2. **Context coherence at turn 45** — a decision made at turn 15 is referenced in a turn-45 edit via the typed `SessionSummary.decisions` field. Measurable via replay.
3. **Structured error rate** — `octos_loop_error_total` distinguishes at least 10 of the 15 `HarnessError` variants in a real session, with correct `recovery_hint` routing.
4. **Credential rotation** under simulated outage — 3 credentials configured, primary exhausts 429s, rotation triggers within 1 RTT, session continues.
5. **Smart routing cost win** — A/B run shows ≥30% token-cost reduction on a mixed coding workload (debugging + writing + questions).
6. **Delegate tool safety** — MAX_DEPTH=2 enforced; grandchild rejected with typed error; parent gets meaningful result via contract-gated delivery.
7. **Compaction semantic preservation** — post-compaction validator confirms declared artifacts still referenced in 100% of runs; LLM-iterative summarizer preserves decisions across 3 consecutive compactions.
8. **Dashboard coverage** — operator diagnoses a stalled session from M6.8 dashboard alone, without reading logs. Test: inject one failure per M6.1-M6.7 surface, verify dashboard shows it.

## Out of scope — explicitly deferred to Track 2 (swarm orchestration)

The following are **not** M6 work. They are the PM+swarm track and will open as a separate family after M6 ships.

- `crates/octos-agent/src/tools/mcp_agent.rs` — calling remote MCP-exposed agents as sub-agents
- `crates/octos-agent/src/mcp_server.rs` — exposing octos sessions as MCP tools for outer orchestrators
- Matrix-as-supervisor-UI (sub-agent Matrix puppets, per-agent rooms)
- Contract authoring + swarm dispatch dashboard (React)
- Cost / provenance ledger
- Agent-chat-style remote-agent-in-tmux pattern
- ACP server / client

M6 is **octos-as-strong-coding-agent**, not octos-as-orchestrator-of-coding-agents. Those are different products; confusing them led to earlier dithering.

## Relationship to M5

M5 (`OCTOS_HARNESS_M5_CODING_RUNNER_CONTRACT.md` on origin/main) introduces `TaskKind` + `CodingHarnessPolicy` + phase classifier + evidence-based completion gate. That is **product correctness** — "the agent can't ghost-succeed." M6 is **loop resilience** — "the agent can complete a 60-turn session without falling over." Complementary.

M5 should land first (per current plan); M6 opens as soon as M5.1/M5.2 give octos a typed coding-session shape to attach M6's improvements to. If M5 is still in progress, the foundations phase M6A can run in parallel without stepping on M5's territory (M6.1 error taxonomy and M6.5 credential pool touch different files than M5's phase classifier).

## What to do now

1. Open 8 contract-bound issues on `octos-org/octos` following the RP / M4 family pattern:
   - `#M6.1` through `#M6.8` — titles from the sub-milestone structure above
2. Create `robotics` / `harness` label siblings: `coding-loop`
3. Author release-slice contract doc `OCTOS_CODING_HARDENING_M6_KICKOFF_2026-04-22.md` (this doc becomes the family plan; kickoff doc is narrower)
4. Dispatch Phase M6A (M6.1 + M6.5) as the first parallel slice
5. Hold Track 2 (swarm orchestrator) until M6 completes

## Summary

- Track 1 = M6 coding loop hardening = **octos as strong standalone coding agent**, via re-expression of hermes innovations through octos's harness primitives
- 8 sub-milestones, ~7.7 kLOC Rust + 1.5 kLOC TypeScript
- 3-4 engineer-months with parallel dispatch
- **Not a port of hermes** — each feature composes with M4.1A events + M4.3 validators + M4.6 ABI + M4.5 dashboard
- Track 2 (PM+swarm orchestrator) deferred until M6 lands
- Success: 60-turn coding session under simulated provider flakiness completes with no human intervention, context coherence preserved, all surfaces observable on dashboard
