# Octos Swarm Positioning — 2026-04-22

See also:

- [OCTOS_COMPETITIVE_LANDSCAPE_2026-04-22.md](./OCTOS_COMPETITIVE_LANDSCAPE_2026-04-22.md)
- [OCTOS_PROTOCOL_STRATEGY_2026-04-22.md](./OCTOS_PROTOCOL_STRATEGY_2026-04-22.md)
- [OCTOS_AGENT_CHAT_PATTERN_2026-04-22.md](./OCTOS_AGENT_CHAT_PATTERN_2026-04-22.md)

## Purpose

Resolve the strategic question "should octos invest in a Claude-Code-class CLI TUI?" and articulate what UI surfaces octos actually needs given the direction AI coding is moving.

## Thesis

The future of AI coding is **program-manager + agent-swarm**, not **human + imperative CLI**. Interaction between humans and agents becomes dispatch-shaped (author a contract, monitor aggregate progress, decide at gates) rather than REPL-shaped. Interaction between agents becomes protocol-mediated (MCP, ACP, A2A) rather than CLI-mediated.

Consequences:

1. **Claude-Code-class CLI TUI is investment in a pattern that's losing share** to dispatch-oriented workflows. Not worthless, but not load-bearing.
2. **Supervisor/PM UI is the growing surface** — and it's dashboard-shaped (web), not TUI-shaped (terminal).
3. **Minimal scripting CLI is still needed** — but `octos admin operator-summary --json` is the right aesthetic, not `ratatui`-animated input areas.

## Evidence

Lived session experience: this document's author has been PM/supervisor for a 17-PR delivery. Across that delivery:

- 10+ parallel implementer agents dispatched, none invoked imperatively at a CLI
- Human input consisted of high-level directives ("prefer C", "review PR 270", "create the issues", "close the merge")
- Translation from directive to swarm dispatch happened in the supervisor layer
- Imperative agent CLI interaction: **zero occurrences**
- Dispatch artifacts used: GitHub issues, contract docs, PR bodies, task-tracker state — all dashboard-shaped, not terminal-shaped

Industry direction:

- Anthropic **Managed Agents** platform — API-first agent orchestration
- Anthropic **Skills** — contract-bound tool extensions
- GitHub **Copilot Workspace** — contract-first task decomposition UI
- **Devin, Cursor agent mode, Amazon Q Developer** — task-shaped interaction
- **agent-chat** (see separate doc) — production system running Claude/Codex in tmux on remote hosts, messaging via MCP + Matrix
- Emerging protocols (MCP, ACP, A2A) — all exist to let agents call agents

Where imperative CLI still dominates:
- Single-developer exploration, debugging, learning
- Local test iteration
- Ad-hoc shell productivity

Real but shrinking share of total AI-coding time.

## Three interface surfaces

When agents call agents, the UI surface splits into three distinct shapes:

### Surface 1 — Agent-to-agent wire protocol

**What**: the format agents use to call other agents.

**Octos today**: MCP client only. Can consume MCP tool servers. Cannot be consumed as an agent; cannot call other agents as agents.

**Octos needed**: MCP-server mode (expose octos sessions as MCP tools for outer orchestrators); MCP-agent-tool backend for `SpawnTool` (call Claude Code / Codex / hermes as sub-agents via MCP). Specifics in [OCTOS_PROTOCOL_STRATEGY_2026-04-22.md](./OCTOS_PROTOCOL_STRATEGY_2026-04-22.md).

**Size**: ~2.5 kLOC Rust, 2-3 engineer-months total for P1 + P2.

### Surface 2 — Program-manager / dispatch UI

**What**: the human-facing UI for authoring contracts, dispatching to swarms, monitoring aggregate, and deciding at gates.

**Shape**: web/desktop dashboard, not terminal TUI. Real-time state via SSE. Contract editor. Swarm dispatch form. Live agent view. PR review surface that understands contract invariants. Cost / provenance ledger.

**Similar products**: Linear for issue tracking, GitHub Projects for orchestration, Devin for agent monitoring. Octos's M4.5 operator dashboard is the start.

**Octos today**: M4.5 landed — reactive/diagnostic dashboard. Shows task lifecycle state, missing-artifact conditions, validator failures. Does NOT yet support: contract authoring, dispatch-to-swarm form, live multi-agent view, review surface.

**Octos needed** (priority-ordered):

1. **Swarm-aware dashboard view** — aggregate multiple concurrent tasks under a dispatched contract, show per-agent progress, surface at-a-glance "which are stuck / succeeded / spending budget."
2. **Contract editor** — syntax-aware editing of workspace policy + RP/M4-family contracts, with validation against the schema version.
3. **Dispatch form** — select contract, select agent pool (local MCP agents, remote MCP servers, sub-octos instances), set budget, fire. Becomes the "New Swarm Run" action.
4. **Cost / provenance ledger** — every agent call attributed to supervisor + contract + task + model + tokens + cost. Essential once swarm size goes from 10 (manual) to 1000 (automated).
5. **PR review hooks** — surface contract invariants, validator evidence (from M4.3), pre-existing latent-issue awareness when reviewing.

**Size**: ~15-25 kLOC React + ~5-8 kLOC Rust backend. 4-6 engineer-months.

### Surface 3 — Minimal scripting CLI

**What**: scriptable CLI for CI, cron, shell pipelines, SSH operator sessions. JSON output. No animation.

**Octos today**: `clap` + `colored = "2"`. Sufficient for the scripting use case. `octos admin operator-summary --json` is the right aesthetic.

**Octos needed**: none incremental. This surface is fine as-is.

**What is *not* needed**: `ratatui`-based animated input areas, prompt_toolkit-equivalent fixed bottom input with streaming-above layout, curses-based interactive multi-select. **That presentation investment goes into Surface 2**, not Surface 3.

## Revised roadmap implication

### Before this strategic frame

Earlier analysis (see [OCTOS_COMPETITIVE_LANDSCAPE_2026-04-22.md](./OCTOS_COMPETITIVE_LANDSCAPE_2026-04-22.md)) identified a ~9.4 kLOC gap between octos and hermes on coding-loop resilience, plus ~2 kLOC on CLI TUI polish.

The instinct: "close both gaps to reach hermes parity."

### After this strategic frame

The CLI TUI gap should **not** be closed. It's investment in a pattern (human-commanding-agent-via-REPL) that the swarm-future compresses.

Revised priorities:

| Original suggestion | Revised priority |
|---|---|
| M5 coding runner contract | Keep — phase classifier + evidence gate are swarm-friendly (supervisor can trust `lifecycle_state == ready && validators pass`) |
| Close hermes CLI TUI gap (~2 kLOC ratatui) | **Skip** — wrong surface |
| Close hermes coding-loop-resilience gap (~9.4 kLOC) | **Defer** — contract-gated delivery is octos's answer to the same problem; redispatch-on-failure is the swarm answer to flaky loops |
| MCP sub-agent tool (octos calls other MCP agents) | **Priority 1** — strongly aligned with swarm future |
| MCP server (octos callable as MCP tool) | **Priority 2** — symmetric value |
| ACP client (octos consumes ACP agents) | Priority 3 — editor-integration niche only |
| ACP server (octos exposes via ACP) | Priority 4 — only if coding-first product pivot |
| Supervisor / PM dashboard (Surface 2 items above) | **Priority 1 track alongside MCP** — unlocks the PM-shaped interaction pattern |
| Cost / provenance ledger | **Priority 2** — needed once swarm size scales |
| Agent-chat-style remote-agent-in-tmux pattern | **Priority 2 alternative track** — see [OCTOS_AGENT_CHAT_PATTERN_2026-04-22.md](./OCTOS_AGENT_CHAT_PATTERN_2026-04-22.md) |

### Two tracks, one direction

The MCP protocol track (agent-to-agent wire) and the supervisor dashboard track (PM-to-swarm UX) are complementary. Both point at the same product bet:

**Octos becomes the orchestration platform that invokes best-in-class agents (Claude Code, Codex, hermes, domain-specific) as sub-agents via MCP, under typed workspace contracts, visible through a supervisor dashboard, with durable artifact delivery and evidence-based completion gating.**

This is a strictly more leveraged position than trying to out-Claude-Code Claude Code on the CLI aesthetic. The CLI aesthetic is where coding was two years ago; the orchestration platform is where coding is going.

## CLI posture decision

**Decision**: do not invest in ratatui/TUI CLI. Keep the scripting CLI minimal. Make the supervisor dashboard the presentation-layer investment.

**Rationale**:
1. The swarm-future pattern reduces human-imperative-CLI usage toward zero
2. Supervisor dashboard captures the interaction that grows (contract authoring + dispatch + review + decide-at-gates)
3. Octos's architectural bet (contract-first, API-first, multi-channel) is natively dashboard-shaped; the CLI was never the product surface
4. The 2 kLOC of ratatui work is non-trivial and ongoing; amortizing that investment against a declining usage pattern is a bad trade
5. Claude Code's own CLI polish is the result of Anthropic treating Claude Code as their own distribution surface — octos does not have that product positioning

**If this decision turns out wrong**, the signal will be: heavy developer demand for an octos CLI with rich live interaction, combined with insufficient coverage of the same workflows in the dashboard. Monitor; don't pre-commit.

## Who is the "human" in a swarm future

In the swarm pattern, the human's job becomes:

1. **Author contracts** — describe what the agent/swarm should deliver, in typed schema
2. **Dispatch to swarms** — set topology, pool, budget, deadline
3. **Monitor aggregate** — which agents green, which stuck, what's burning budget, what's the drift from contract
4. **Decide at gates** — merge this? rescope that failure? redispatch repair? escalate?
5. **Review outputs** — PR diffs, test results, validator evidence, cost attribution
6. **Triage failures** — drill into an agent's actual work when the aggregate report shows an anomaly

Zero of these six fits a terminal REPL. All six fit a dashboard.

The surviving CLI use case is:

7. **Scripting / CI / cron** — run supervisor-authored workflows on a schedule, with JSON output that feeds other tooling

This is Surface 3, which octos already handles adequately.

## Non-trivial prediction

**2 years out** (mid-2028): the human's role compresses further, from swarm supervisor to program portfolio manager. The PM-of-agents becomes itself machine-executable — the human then approves programs, approves budgets, approves merges. The interaction becomes even more dashboard-shaped.

If this prediction holds, the order-of-investment for octos is:
1. Today: contract authoring + swarm dispatch dashboard
2. +1 year: programs-of-contracts (many related contracts dispatched as a unit)
3. +2 years: portfolio view of multiple concurrent programs across teams

Each layer is more dashboard-shaped than the last. Each is further from the CLI TUI pattern.

## Caveat

The swarm pattern today requires **very capable individual agents**. Claude Opus 4.7 is at the frontier of what can act as both supervisor AND worker. A year ago this wouldn't have worked. If model progress slows, some workloads fall back to human-in-the-loop with stronger CLI UX expectations. Watch model capability — if plateau, reopen the CLI question.

## Summary

- Swarm pattern is the forward direction
- Three UI surfaces diverge: wire protocol, supervisor dashboard, scripting CLI
- Claude-Code-class TUI is the dying pattern; skip that investment
- Supervisor dashboard is the growing pattern; invest there
- Scripting CLI is sufficient; leave it
- MCP (not ACP) is the protocol for agent-to-agent composition in the swarm future
- Octos's architectural bet is already aligned; doubling down on that direction is the winning move
