# Agent-Chat Pattern — Remote Coding Agent Reference Architecture

See also:

- [OCTOS_PROTOCOL_STRATEGY_2026-04-22.md](./OCTOS_PROTOCOL_STRATEGY_2026-04-22.md)
- [OCTOS_SWARM_POSITIONING_2026-04-22.md](./OCTOS_SWARM_POSITIONING_2026-04-22.md)

## Purpose

Document a real production reference architecture (`~/home/agent-chat/`) that runs Claude and Codex as remote coding agents using **tmux + MCP + Matrix + SSE**. This pattern is a concrete answer to "how do you deploy coding agents remotely today without ACP." Extract lessons and surface implications for octos's protocol and UX strategy.

## Summary

Agent-chat is a multi-agent orchestration platform where Claude and Codex CLIs run inside tmux sessions on multiple servers. Each agent has its own MCP server that talks to a central backend over HTTPS. Humans interact via Matrix (with agent-puppet rooms). A central backend mediates all state (agents, messages, tasks, alerts, supervisor-audit). SSE broadcasts events; per-server push-relays inject notifications directly into tmux panes. No ACP anywhere.

This works because:

1. MCP transports include HTTP, which trivially crosses hosts
2. tmux is a durable, user-owned, remote-accessible shell context — perfect substrate for an agent to occupy
3. Matrix is a federated chat protocol with existing human-facing clients
4. The backend is the source of truth; agents and UI layers are projections

The architecture is ~6.5 MB of Node.js on disk. Several production systemd services run it.

## Topology

```
                    ┌──────────────────────────────────────────┐
                    │  Central server                          │
                    │                                          │
                    │  backend-v2.js  (:8090)  ← Source of      │
                    │                            truth         │
                    │  server.js      (:8084)  ← Dashboard UI   │
                    │  bridge-matrix.js         ← Matrix bridge│
                    │  push-relay.js            ← Local tmux   │
                    │  mcp-server.js (per-agent)               │
                    └───────────────────┬──────────────────────┘
                                        │
                                        │ HTTPS + SSE
                                        │
           ┌────────────────────────────┼────────────────────────────┐
           │                            │                            │
  ┌────────▼───────┐          ┌─────────▼──────┐          ┌──────────▼────┐
  │ Remote host 1  │          │ Remote host 2  │          │ Remote host N │
  │                │          │                │          │               │
  │ push-relay     │          │ push-relay     │          │ push-relay    │
  │ mcp-server(s)  │          │ mcp-server(s)  │          │ mcp-server(s) │
  │                │          │                │          │               │
  │ tmux sessions: │          │ tmux sessions: │          │ tmux sessions:│
  │  - Claude A    │          │  - Codex C     │          │  - Claude D   │
  │  - Codex B     │          │                │          │  - Codex E    │
  └────────────────┘          └────────────────┘          └───────────────┘
```

Each Claude/Codex instance runs inside tmux. Each instance has its own MCP server attached; the agent's tool calls land on that MCP server, which forwards to the central backend via HTTPS.

## Message flow

```
Agent A (Claude/Codex, inside tmux pane on Remote Host 1)
  │
  │ tool call: send_message(to=agent-b, body=...)
  │
  ▼
mcp-server.js (local to Agent A)
  │
  │ POST /api/messages  (agent-token auth)
  │
  ▼
backend-v2.js (central)
  │
  ├──▶ persists message to store
  │
  ├──▶ broadcasts SSE event
  │       │
  │       ▼
  │   push-relay.js (Remote Host 2, listening on SSE)
  │       │
  │       │ injects notification into Agent B's tmux pane
  │       │ via `tmux send-keys` or similar
  │       │
  │       ▼
  │   Agent B (Claude/Codex) sees notification in its terminal
  │   → reads via its own mcp-server's inbox tool
  │
  └──▶ bridge-matrix.js receives same SSE event
         │
         ▼
       relays to Matrix room for human visibility
```

Key properties:

- **Backend is the single source of truth** for agents, groups, messages, cursors, tasks, alerts
- **MCP is the agent-side protocol** — every agent speaks MCP to its local mcp-server, which speaks HTTPS to backend
- **SSE is the broadcast substrate** — low-overhead, one-way push
- **tmux injection is the local push** — agent notifications arrive as literal keystrokes in the agent's terminal
- **Matrix is the human UI** — agents have puppet users; humans see agents in Matrix rooms

## Backend API surface

The `backend-v2.js` exposes ~60 REST endpoints across 8 domains:

| Domain | Endpoints | Role |
|---|---|---|
| Servers | heartbeat, offline, maintenance, list | Multi-server liveness tracking |
| Agents | register, list, get, patch, delete, undelete, offline, runtime, avatar, groups, tasks | Agent registry + runtime state |
| Runtime | compact, push-delivered | Compaction events + delivery confirmations |
| Groups | create, list, get, members, delete, messages | Group messaging |
| DM | ensure | Direct-message channel setup |
| Messages | send, get, suppress | Message store |
| Inbox | get (advances cursor), unread, unread-list | Per-agent inbox cursors |
| Media | stage, fetch | Base64 media upload |
| System | info | System event log |
| Tasks | create, list, get, patch, delete, accept, transition, execution | Task store with FSM |
| Task graphs | create, list, get, delete, node-patch | DAG orchestration |
| Alerts | list, stats, get, transition, notes, patch, delete | Alert ticket system |
| Supervisor | status, agents, control, state, heartbeat | LLM-based focus audit |
| Subconscious | events, detail, upstream hooks | Claude subconscious/hooks |

This is a **fully-realized orchestration platform**, not a prototype. Features include:

- Agent online/offline with `manualDown` (intentional shutdown vs crash)
- MCP presence detection (`mcpPresent`)
- Systemd scope memory monitoring with pressure alerts
- Swap usage alerting
- Deletion tombstones with undelete
- DM + group messages with `request`/`inform`/`reply` types
- Priority levels (`normal`/`high`/`urgent`)
- Structured message schemas
- Task FSM: `created → accepted → in_progress → done`; `in_progress ↔ blocked`
- Task graph DAGs with node dependencies
- Alert ticket FSM with deduplication, auto-resolution, reopen windows
- Authentication: bearer token + agent-token + bridge-secret + subconscious-token
- Supervisor: LLM-based focus evaluation every 30s with role/boundary/task context

## Key components

### `backend-v2.js` (central, :8090)
The API server. Source of truth. All data lives here. ~60 REST endpoints, SSE broadcast.

### `server.js` (central, :8084)
Dashboard UI + message queue + idle detection + reminders + alert UI. The web-facing front door for humans (in addition to Matrix).

### `bridge-matrix.js` (central)
Matrix bridge. Creates agent puppet users. Maps DMs and groups to Matrix rooms. Relays messages both directions. Configured with `MATRIX_BRIDGE_SECRET`.

### `push-relay.js` (per-server)
SSE consumer → tmux notification injection. Runs on every host with agents. Subscribes to backend SSE. On each relevant event, injects into the target agent's tmux pane via the tmux CLI. The agent sees the notification as part of its own terminal context, which Claude/Codex will naturally consume on the next iteration.

### `mcp-server.js` (per-agent)
MCP server exposing agent-chat's messaging/task/alert tools to the agent (Claude or Codex). On the central host, connects directly. On remote hosts, connects via HTTPS. Agent invokes `send_message`, `read_inbox`, `create_task`, etc. through its normal MCP tool-calling mechanism.

### `bin/agentchat`, `bin/agent-up`, `bin/agent-down`, etc.
CLI lifecycle tools for operators. Create/start/stop/list agents. Register with backend.

## Why this architecture wins for remote coding agents

### MCP-over-HTTPS enables natural multi-host scaling

Every remote agent's MCP server has a central backend URL over HTTPS. No special transport layer; no stdio-binding constraint. This is exactly the advantage MCP has over ACP — remote agents are a first-class deployment pattern.

### tmux as durable shell context

tmux sessions survive SSH disconnects, reboots (with tmux-resurrect or systemd supervision), and supervisor restarts. An agent that occupies a tmux session has a durable workspace without the agent implementation itself needing to handle persistence of the shell state. This is a huge simplification — the agent just runs; tmux handles the "where does it live" question.

### Push-relay pattern for cross-host notifications

Rather than require agents to poll, the central backend broadcasts via SSE and per-server push-relays inject into local tmux sessions. This crosses process/host boundaries via simple push semantics. Latency is low (one SSE hop) and the mechanism is trivial compared to a custom RPC protocol.

### Matrix as the human UI

Humans don't need a bespoke dashboard for individual agent chats — Matrix already solves federated, durable, multi-client, encryption-capable chat. Agent-chat creates puppet users; Matrix does the rest. The human can respond from a phone, desktop, or Element web client. This is the Surface 2 (supervisor UI) problem solved by piggy-backing on existing infrastructure.

### Agent-token auth

Each agent has its own token (`X-Agent-Token` header). Agent-specific routes (runtime updates, task transitions, alert transitions for assigned alerts) enforce agent identity. The token mode supports `hard` (enforce), `audit` (log only), `off` (disabled) — incremental hardening.

### Supervisor with LLM-based focus audit

Every 30s, the supervisor assesses active agents using LLM evaluation against role/boundary/task context. Consecutive negative ratings trigger nudge (to agent) then escalation (to operator). This is an interesting **meta-agent pattern** — an LLM watching other LLMs and nudging them back on track. Analogous to M4.3's validator runner but at a higher level of abstraction (agent behavior, not artifact validity).

## What agent-chat does NOT do

Explicitly absent:
- **No ACP** — not as client, not as server. Entire system avoids ACP.
- **No typed workspace contract** — messages are structured (schema.kind, schema.payload) but not contract-gated
- **No artifact gate** — no analogue of octos's M4.1A workspace contract
- **No evidence-based completion gate** — no analogue of M4.3 validator runner
- **No executable skill binaries** — skills live as markdown under `skills/` (advisory, like hermes)
- **No declarative validation policy** — validation happens in prose or in tests invoked by agents

So agent-chat is **orchestration + communication + monitoring**, not **typed contract delivery**. Those are complementary, not competing, axes.

## Lessons for octos

### Lesson 1 — MCP-over-HTTPS is the remote-agent substrate

Agent-chat validates the MCP protocol strategy (see [OCTOS_PROTOCOL_STRATEGY_2026-04-22.md](./OCTOS_PROTOCOL_STRATEGY_2026-04-22.md)). HTTPS transport + HTTPS auth headers = remote agents Just Work. This is **exactly the pattern octos should adopt** for Priority 1 (octos calls Claude Code / Codex / hermes as MCP sub-agents) and Priority 2 (octos exposes itself via MCP server).

Concretely: when octos's `SpawnTool` gains an `agent_mcp` backend, it should support **URL-based agent addressing**, not just stdio-subprocess. A task can dispatch to a **remote** MCP-agent endpoint as easily as a local subprocess. This makes octos-orchestrated swarms multi-host from day one.

### Lesson 2 — tmux as optional agent substrate

For agents that benefit from a persistent shell context (interactive debugging, long-running compiles, stateful tool sessions), tmux-hosted agents are a proven pattern. Octos's `SpawnTool` currently spawns processes directly. A future extension could support **tmux-hosted spawn**: spawn the agent inside a tmux session, inject progress via tmux send-keys, expose the session for human visibility via SSH.

This is **not** a priority but is a useful pattern to have on the roadmap when developer workflows demand it.

### Lesson 3 — Matrix as supervisor UI transport

Octos already has a Matrix channel (`crates/octos-bus/src/matrix_channel.rs`, 1,600+ LOC per prior review). That channel is currently oriented toward user-facing chat for a single agent. The agent-chat pattern shows a different use: **agents register as Matrix puppets**, and each agent's activity flows into a Matrix room that humans join.

Octos could extend its Matrix support to:
- Register spawned sub-agents as Matrix puppets
- Route sub-agent progress events to per-agent Matrix rooms
- Give the supervisor a federated, encrypted, multi-client UI without building a bespoke dashboard

This is **Surface 2 solved by existing infrastructure**. Bigger win than a custom React swarm view for many enterprise deployments.

### Lesson 4 — Backend-as-source-of-truth with projections

Agent-chat's architecture cleanly separates:
- **Backend** (state)
- **SSE** (change notification)
- **Projections** (Matrix bridge, dashboard UI, push-relay, MCP server per-agent)

Octos's current design mixes these more. `octos serve` is both the REST backend AND the dashboard host. The channel bus is both transport AND persistence.

If octos evolves toward the swarm pattern (many sub-agents, many projections), separating the source-of-truth from the projections would help. Current state of the M4 landing has already moved this direction (e.g., operator dashboard at M4.5 reads existing backend APIs, no UI-side state duplication). Continue in that direction.

### Lesson 5 — Hook into agent subconscious / audit

Agent-chat has a `subconscious` surface for Claude's hook events: `/api/subconscious/upstream/bootstrap`, `session-start`, `user-prompt`, `pretool`, `stop`. This is Anthropic's own hook format; agent-chat receives these as first-class ingestion events.

Octos's `hooks.rs` has a similar pattern at the runtime level. **Supporting the Claude-Code-native hook format** (in addition to octos's own hook events) would let octos-hosted Claude Code instances contribute their introspective data back to octos's supervisor. That's a cheap integration once MCP-agent-tool is in place.

### Lesson 6 — Supervisor-as-LLM-watcher

Agent-chat's supervisor uses an LLM to audit agent focus. Octos doesn't have this pattern. An LLM-based "meta-supervisor" that monitors active sub-agents, nudges them back on task when drift is detected, and escalates to human operator is a natural extension of M4.3 validator runner — not a turn-level validator, but a session-level one. Consider for future roadmap.

## What octos could absorb

Pragmatic integration path:

1. **Immediately usable**: MCP-over-HTTPS remote agent dispatch — absorb into Priority 1 from protocol strategy
2. **Short-term** (quarters): Matrix-as-supervisor-UI pattern — extend existing Matrix channel with multi-agent projection
3. **Medium-term** (year): tmux-hosted agent option for `SpawnTool`
4. **Long-term**: LLM-based focus-audit supervisor — session-level extension of validator runner

These are all **additive to octos's contract-first posture**. They strengthen the swarm-orchestration capability without compromising the workspace-contract / artifact-gate differentiation.

## Relationship to hermes and openJiuwen

Agent-chat is orthogonal to both:

- **Hermes** is a single-agent specialist (`run_agent.py` 10,492 LOC for one deep loop). Agent-chat doesn't replace hermes; it could USE hermes as an agent-inside-tmux, calling hermes via MCP the same way it calls Claude or Codex.
- **OpenJiuwen** has web Studio for agent creation and TeamAgent for swarm, but lacks agent-chat's specific remote-tmux-hosting and SSE-push-to-tmux-injection. OpenJiuwen and agent-chat are different swarm patterns — openJiuwen's is more "design and deploy" while agent-chat's is more "orchestrate the running ones."

Agent-chat sits between them: lighter than openJiuwen's full Studio, broader than hermes's single agent. **Octos's natural position is to take agent-chat's orchestration pattern and combine it with openJiuwen-like contract-first deployment + hermes-like agent-as-sub-agent invocation.**

## Concrete next-action proposals

### 1. `crates/octos-mcp-remote/` — Remote MCP agent dispatch

Extend `SpawnTool` with a `remote_mcp_agent` backend:

- Task dispatches to `https://agents.example.com/claude-code/mcp` (or similar)
- Authentication: bearer token per agent endpoint
- Request-response shaped task (MCP `tools/call`)
- Result flows back through octos's workspace-contract path
- Progress observed via M4.1A harness events (or optionally pulled from a companion SSE endpoint)

Scope: ~1 kLOC Rust (building on existing MCP client). Aligns with Priority 1 of the protocol strategy doc; the agent-chat pattern shows the deployment shape this should support.

### 2. Matrix-as-supervisor-UI — Extend existing Matrix channel

Per-agent Matrix room when sub-agent spawns. Agent progress events flow into the room. Human supervisor interacts from any Matrix client. Register sub-agents as Matrix puppets.

Scope: ~500-800 LOC Rust in `octos-bus/src/matrix_channel.rs`. Builds on existing implementation.

### 3. Session supervisor with LLM focus audit (optional, deferred)

An LLM evaluates session focus against contract invariants. Acts as a meta-validator beyond M4.3's per-artifact checks. Nudges or escalates.

Scope: ~1.5 kLOC Rust. Only if demand signal appears.

## Summary

- Agent-chat is a production reference architecture for remote coding agents
- Key primitives: MCP (protocol), tmux (shell substrate), Matrix (human UI), SSE (change notification), central backend (source of truth)
- ACP absent; not needed
- Validates MCP-over-HTTPS as the remote-agent deployment pattern
- Octos should absorb lessons: MCP-remote dispatch, Matrix-as-supervisor-UI, tmux-hosted agent option, LLM focus audit
- These additions strengthen octos's swarm-orchestration story without changing its contract-first differentiation
