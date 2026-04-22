# Octos Swarm Orchestrator — M7 Plan (Track 2)

See also:

- [OCTOS_SWARM_POSITIONING_2026-04-22.md](./OCTOS_SWARM_POSITIONING_2026-04-22.md)
- [OCTOS_PROTOCOL_STRATEGY_2026-04-22.md](./OCTOS_PROTOCOL_STRATEGY_2026-04-22.md)
- [OCTOS_AGENT_CHAT_PATTERN_2026-04-22.md](./OCTOS_AGENT_CHAT_PATTERN_2026-04-22.md)
- [OCTOS_CODING_HARDENING_M6_PLAN_2026-04-22.md](./OCTOS_CODING_HARDENING_M6_PLAN_2026-04-22.md) — Track 1

## Purpose

Turn octos from a strong standalone coding agent (Track 1 / M6) into a **PM + swarm orchestrator**. Under the swarm-future thesis (humans author contracts, agents call agents), octos's architectural bet is already contract-first, API-first, schema-versioned — M7 adds the connective tissue that makes octos call best-in-class coding agents (Claude Code, Codex, hermes) as sub-agents, exposes octos sessions to outer orchestrators as callable tools, and surfaces swarm state to supervisors.

## Why M7 can ship in parallel with M6 merge gate

M6 work (8 PRs pending merge) touches core agent-loop files: `loop_runner.rs`, `execution.rs`, `plugins/tool.rs`, plus additive extensions to `harness_events.rs`, `task_supervisor.rs`, `metrics.rs`.

M7 work is almost entirely **new files** in different modules:
- `crates/octos-agent/src/tools/mcp_agent.rs` (new)
- `crates/octos-agent/src/mcp_server.rs` (new)
- `crates/octos-cli/src/swarm/` (new module)
- `crates/octos-bus/src/matrix_channel.rs` (extend — M6 doesn't touch)

Shared files (`harness_events.rs`, `metrics.rs`, `task_supervisor.rs`, `abi_schema.rs`, `lib.rs`) remain additive-only — M7 adds new variants + counters + re-exports, the same pattern the integration-reconciler already resolved cleanly for M6.

**M7 Wave 1 dispatches immediately**. Subsequent waves follow M6's merge cadence.

## Architectural thesis

Octos at end of M7:

- **As orchestrator**: `SpawnTool` gains an MCP-agent backend. Configured agent binaries (`claude mcp serve`, `codex mcp serve`) or remote MCP endpoints (`https://agents.example.com/claude-code/mcp`) become contract-bound sub-agents. Task dispatched → MCP `tools/call` → sub-agent runs its own loop → result flows back through workspace contract → parent never sees sub-agent internal state (context isolation win).
- **As callable worker**: octos exposes running sessions as MCP tools. Outer orchestrators (another octos instance, Codex, Claude Code, hermes, jiuwenclaw) can invoke octos via the same `tools/call` API.
- **As swarm supervisor**: First-class `Swarm::dispatch(contracts, topology, budget) -> SwarmResult` primitive replaces the manual orchestration pattern supervisors run today. Hybrid: humans authoring contracts, swarm primitive fanning out.
- **As visible to humans**: Matrix-as-supervisor-UI (extending existing channel) registers sub-agents as puppets, routes progress events to per-agent rooms. The human supervises via Element (desktop / mobile / web) — zero net-new UI code. Plus contract-authoring dispatch form in the existing React dashboard.

Each piece is typed, schema-versioned, validator-gateable. Consistent with octos's existing harness posture.

## Sub-milestones

Eight contract-bound issues, same RP / M4 / M6 family style.

### M7.1 — MCP agent-tool backend for SpawnTool

**Problem**: No way to call external MCP-exposed agents (Claude Code, Codex, hermes) as sub-agents. Today `SpawnTool` only spawns configured `app-skill` binaries.

**Deliverable**: `crates/octos-agent/src/tools/mcp_agent.rs` (new). Extends `SpawnTool` config with `agent_mcp` backend variant. Supports both local (stdio subprocess, e.g., `claude mcp serve`) and remote (HTTPS, e.g., `https://agents.example.com/claude-code/mcp`) MCP endpoints. Request-response via `tools/call`. Result routed through workspace contract → M4.1A artifact delivery. Context isolation: sub-agent's internal state stays inside the MCP call; only final result lands in parent context.

**Size**: ~1,200 LOC Rust.

**Dependencies**: Existing MCP client in `octos-agent`; M4.1A contract-gated delivery (landed).

### M7.2 — MCP server mode (octos as callable tool)

**Problem**: No way for outer orchestrators (another octos, Codex, Claude Code) to call octos as a sub-agent. Octos's REST API exists but isn't MCP-shaped.

**Deliverable**: `crates/octos-agent/src/mcp_server.rs` (new). Exposes octos sessions as MCP tools. Session-level exposure (one MCP tool = one full octos session that runs to completion and returns the workspace-contract artifact). Supports stdio transport (default), HTTP (for remote). Authentication: stdio parent-trust, HTTP bearer token.

**Size**: ~1,500 LOC Rust.

**Dependencies**: Existing MCP protocol types in `octos-agent`; existing session machinery.

### M7.3 — Matrix-as-supervisor-UI

**Problem**: Supervisor has no live view of multiple concurrent sub-agents without a bespoke dashboard. Agent-chat pattern (`~/home/agent-chat/`) validates using Matrix for this.

**Deliverable**: Extend `crates/octos-bus/src/matrix_channel.rs`. On sub-agent spawn, register Matrix puppet user (`@<agent-label>-<session>:homeserver`) and per-swarm room. Route M4.1A harness events to the room as messages from the corresponding puppet. Human replies in the room route back to the sub-agent as steering input. Supervisor uses Element / Fluffy / any Matrix client — zero net-new UI code.

**Size**: ~800 LOC Rust (extends existing 1,600+ LOC matrix channel).

**Dependencies**: None. Can dispatch in Wave 1 in parallel with M7.1 / M7.2.

### M7.4 — Cost / provenance ledger

**Problem**: No attribution for swarm dispatches. Once supervisor spawns N sub-agents for a contract, budget accountability disappears.

**Deliverable**: New ledger per dispatch: `supervisor_session_id + contract_id + task_id + model + tokens_in/out + cost + timestamp`. Redb-backed persistent store (same pattern as M6.5 credential pool). Emitted as `octos.harness.event.v1 { kind: "cost_attribution" }`. Operator summary extension with per-contract cost breakdown. Optional budget enforcement (fail dispatch if projected cost exceeds budget).

**Size**: ~1,000 LOC Rust.

**Dependencies**: M7.1 + M7.2 (to have dispatches to attribute).

### M7.5 — Swarm orchestration primitive

**Problem**: Supervisor orchestration pattern (author contract → fan out N agents → aggregate → report) is manual today. PM wrote it by hand during RP and M6. Should be a first-class API.

**Deliverable**: New `crates/octos-swarm/` crate with `Swarm::dispatch(contracts, topology, budget) -> SwarmResult`. Topology types: `Parallel(n)`, `Sequential`, `Pipeline`, `Fanout(Pattern)`. Records dispatch provenance, aggregates artifacts, runs M4.3 validator on aggregate output, rolls up cost via M7.4 ledger. Re-entry on partial failure (redispatch failed sub-contracts). Session-durable (supervisor can reload state after crash).

**Size**: ~2,500 LOC Rust (new crate).

**Dependencies**: M7.1, M7.4.

### M7.6 — Contract authoring + swarm dispatch dashboard

**Problem**: Dashboard is diagnostic (M4.5 / M6.8). Supervisor needs to author contracts + dispatch + monitor + decide-at-gates.

**Deliverable**: `dashboard/src/pages/SwarmPage.tsx` + supporting components:
- Contract editor (syntax-highlighted JSON/TOML)
- Dispatch form (select contract, pool, topology, budget)
- Live swarm view (per-agent progress, cost, lifecycle state)
- PR review surface (contract invariant checks + M4.3 validator evidence + cost attribution)
- Integration with M7.5 Swarm primitive

**Size**: ~6,000 LOC React + ~1,500 LOC Rust backend (additive to admin API).

**Dependencies**: M7.1, M7.4, M7.5.

### M7.7 — ACP client adapter (deferred niche)

**Problem**: Editor integration (Zed, Claude Code editor mode) uses ACP. Octos-as-ACP-agent lets editors invoke octos as the coding agent.

**Deliverable**: `crates/octos-agent/src/acp_client.rs` + `acp_server.rs`. ACP JSON-RPC over stdio. Maps ACP session lifecycle to octos session. Streams M4.1A harness events as ACP progress frames.

**Size**: ~3,000 LOC Rust.

**Dependencies**: None, but LOW priority per protocol-strategy doc. Deferred unless editor-integration demand signals.

### M7.8 — Live fleet gate + release validation

**Problem**: Need a repo-side gate that validates swarm dispatch end-to-end on real canary (analogous to M4.1A.474 live gate).

**Deliverable**: `scripts/validate-m7-swarm-live.sh` + `e2e/tests/swarm-dispatch-gate.spec.ts`. Authors fixture contract, dispatches via M7.5 Swarm primitive, verifies: sub-agents spawned, progress events flowed, artifacts delivered, cost ledger attributed, Matrix rooms created, validator ran, reload preserves state. Exit codes with structured diagnostic JSON.

**Size**: ~800 LOC bash + TypeScript.

**Dependencies**: M7.1-M7.6 all merged.

## Phasing + parallel dispatch

```
Phase M7A (parallel, Wave 1)       ← dispatches NOW
┌──────────────────────────────────────┐
│ M7.1 MCP agent-tool backend          │  ← 3-4 weeks, new file
│ M7.2 MCP server mode                 │  ← 3-4 weeks, new file
│ M7.3 Matrix-as-supervisor-UI         │  ← 2 weeks, extends existing matrix channel
└──────────────────────────────────────┘
                │
                ▼
Phase M7B (parallel, Wave 2)       ← after M7A merges
┌──────────────────────────────────────┐
│ M7.4 Cost / provenance ledger        │  ← depends on M7.1+M7.2
│ M7.5 Swarm orchestration primitive   │  ← depends on M7.1+M7.4
└──────────────────────────────────────┘
                │
                ▼
Phase M7C (Wave 3)
┌──────────────────────────────────────┐
│ M7.6 Contract authoring + dashboard  │  ← depends on M7.4+M7.5
└──────────────────────────────────────┘
                │
                ▼
Phase M7D (optional / deferred)
┌──────────────────────────────────────┐
│ M7.7 ACP client (editor niche)       │  ← deferred unless demand
└──────────────────────────────────────┘
                │
                ▼
Phase M7E (final gate)
┌──────────────────────────────────────┐
│ M7.8 Live fleet gate                 │  ← after M7.1-M7.6
└──────────────────────────────────────┘
```

**Total**: ~14 kLOC Rust + ~6 kLOC TS, 4-6 engineer-months with parallel dispatch.

## Success criteria

M7 is complete when all of the following hold:

1. **Octos dispatches Claude Code as sub-agent**: configured `claude mcp serve` endpoint, octos calls `tools/call` with a typed contract, Claude Code executes its internal loop, result returns through workspace contract, parent context remains small (under 5 kB of sub-agent telemetry).
2. **Octos exposes itself as MCP tool**: `octos serve --mcp` mode accepts `tools/call` from outer orchestrator, runs session, returns contract-gated artifact.
3. **Remote agent dispatch works**: URL-based MCP endpoint (HTTPS) dispatches identically to local stdio.
4. **Matrix supervisor UI live**: supervisor joins per-swarm Matrix room on Element mobile, sees 3 sub-agents progress, replies to steer one, reply routes to correct sub-agent.
5. **Swarm primitive**: `Swarm::dispatch` call fans out N contracts, aggregates, rolls up cost via ledger, runs validator on aggregate.
6. **Dashboard contract-authoring**: supervisor writes contract in `SwarmPage`, dispatches with one click, sees live progress + cost + validator evidence.
7. **Live fleet gate**: `scripts/validate-m7-swarm-live.sh --base-url https://dspfac.crew.ominix.io` runs end-to-end + exits 0.
8. **No M6 regressions**: all M6 tests remain green; shared files (`harness_events.rs`, `metrics.rs`, `task_supervisor.rs`) only grow.

## Out of scope — explicitly deferred

- ACP server (octos-as-ACP-agent) — unless coding-first product pivot
- A2A protocol — only if enterprise agent-to-agent demand materializes
- Multi-tenant cost policy enforcement beyond per-contract ceiling
- `slam-nav-sim` or robotics-specific swarm templates (RP family separately)
- SWE-bench orchestration (hermes has this; if octos needs it, separate milestone)

## Hermes comparison (re-expression principle preserved)

| Hermes coding capability | M7 re-expression |
|---|---|
| ACP server + Copilot ACP client | MCP-first: M7.1 + M7.2 with optional ACP deferred to M7.7 |
| Sync `delegate_task` with MAX_DEPTH=2 | Already in M6.7; M7.1 extends to external agents via MCP |
| Full Python `run_agent.py` monolith | Typed `SpawnTool.agent_mcp` + `Swarm::dispatch` — composable primitives |
| No central budget / cost attribution | M7.4 cost ledger with per-contract + schema-versioned |
| No supervisor UI (hermes is CLI-first) | M7.3 Matrix puppet pattern + M7.6 React dispatch dashboard |

Every M7 feature composes with M4.1A events, M4.3 validators, M4.5 dashboard, M6 coding-loop hardening. No parallel implementations.

## Conflict surface analysis vs in-flight M6 work

| File | M6 touches | M7 touches | Conflict risk |
|---|---|---|---|
| `crates/octos-agent/src/tools/mcp_agent.rs` | — | M7.1 (new) | None |
| `crates/octos-agent/src/mcp_server.rs` | — | M7.2 (new) | None |
| `crates/octos-swarm/` | — | M7.5 (new crate) | None |
| `crates/octos-bus/src/matrix_channel.rs` | — | M7.3 (extends) | None |
| `crates/octos-agent/src/harness_events.rs` | M6.1/2/3/5/6 add variants | M7.1/7.4/7.5 add variants | Additive — integration-reconciler pattern |
| `crates/octos-cli/src/api/metrics.rs` | M6 adds counters | M7 adds counters | Additive |
| `crates/octos-agent/src/task_supervisor.rs` | M6 adds match arms | M7.5 adds variants to match | Additive |
| `dashboard/src/pages/HarnessPage.tsx` | M6.8 extends | M7.6 adds new page | None |

**M7A dispatches immediately.** M7B+ waves fire after M7A merges on the same additive-conflict resolution pattern the integration-reconciler proved for M6.

## Governance

Same as M6 family: contract-bound issues with allowed-files lists, required invariants, named acceptance tests. PM gate-QA on agent return. Integration-branch composition verifies Wave-1 + Wave-2 before human merge gate.

## Next actions

1. Open 8 contract-bound issues (M7.1 through M7.8)
2. Create 3 worktrees from origin/main for Wave 1 (M7.1, M7.2, M7.3 — all disjoint)
3. Dispatch 3 parallel background implementers
4. Gate QA + open PRs as agents return
5. After Wave 1 merges, dispatch Wave 2 (M7.4, M7.5)
6. After Wave 2 merges, dispatch M7.6
7. After M7.6 merges, optionally dispatch M7.7 (editor niche) + M7.8 (live gate)

Total dispatch-to-completion estimate with swarm-scale orchestration: 1-2 PM sessions. With sequential engineering: 4-6 months.
