# Octos Protocol Strategy — 2026-04-22

See also:

- [OCTOS_COMPETITIVE_LANDSCAPE_2026-04-22.md](./OCTOS_COMPETITIVE_LANDSCAPE_2026-04-22.md)
- [OCTOS_SWARM_POSITIONING_2026-04-22.md](./OCTOS_SWARM_POSITIONING_2026-04-22.md)

## Purpose

Record octos's strategic posture on agent-interchange protocols: **MCP**, **ACP**, and **A2A**. Document the competitive dynamics between Anthropic, Zed, Google, and third-party orchestrators. Define the recommended octos protocol roadmap.

## Protocols in play

### MCP — Model Context Protocol

- Published by **Anthropic** (November 2024)
- Anthropic-controlled spec
- Architecture: LLM host ↔ tool/data **server**
- Exposes: `tools` (callable functions), `resources` (readable data), `prompts` (reusable prompts)
- Transport: stdio, SSE, HTTP
- Orchestration ownership: **host** decides when/how to call each tool

### ACP — Agent Client Protocol

- Originated with **Zed** editor for AI coding-agent integration
- Anthropic supports it for Claude Code editor integration (but deliberately not as the primary sub-agent exposure mechanism)
- Architecture: editor/outer-client ↔ full **agent**
- Flows: initialize, prompt, streaming thinking + tool calls + results + replies, session fork/load, authentication, MCP-server-forwarding
- Transport: JSON-RPC; in practice always stdio (client spawns agent as subprocess)
- Orchestration ownership: **agent** owns the full loop internally; client just sends user turns and reads streams

### A2A — Agent-to-Agent Protocol

- Published by **Google**
- Architecture: agent ↔ agent, peer-to-peer
- Positioned as an open standard for multi-agent orchestration
- OpenJiuwen has a complete C++ SDK; Anthropic and OpenAI have not adopted publicly

## Does ACP require the sub-agent to run on the same computer?

**In practice today: yes. By spec: no.**

ACP is JSON-RPC over a transport. The transport is not fixed by the spec — it could be stdio, TCP, WebSocket, or HTTP. But every production implementation uses stdio: the client spawns the agent as a child process and communicates over stdin/stdout.

Examples:
- Zed calling Claude Code: Zed spawns `claude --acp` as a subprocess
- hermes's `copilot_acp_client.py`: `subprocess.Popen(["copilot", "--acp"], ...)` with pipes
- hermes's own ACP server (`acp_adapter/server.py`): runs under `hermes acp` as an stdio JSON-RPC server

Consequences of stdio-default:
- Same machine, same user, shared filesystem
- Simple authentication (parent process spawned you, so trusted)
- Cannot scale across hosts
- No network auth layer needed
- Agent has access to editor's working directory

For **remote** ACP you would need either a TCP/WebSocket transport shim (nothing in the spec forbids it) or a relay pattern. No standard implementation provides this today.

**Practical implication**: ACP is a desktop/developer-workstation protocol. Multi-tenant server deployments need a non-ACP orchestration path (REST, gRPC, or a networked-transport extension of ACP).

## MCP vs ACP — the substance

### Technical comparison

| Dimension | MCP | ACP |
|---|---|---|
| What is standardized | LLM ↔ tool/data server | Editor/client ↔ agent |
| Who is the server | Tool/resource provider (DB, filesystem, API) | The agent itself |
| Who is the client | LLM host (Claude Desktop, Claude Code, Codex, …) | Editor or outer orchestrator |
| What flows | `tools`, `resources`, `prompts`; host chooses when to call | Full agent session: prompt, reply, tool-call streaming, session fork |
| Orchestration | **Host** owns the loop | **Agent** owns the loop |
| Context footprint in caller | Small (just tool I/O) | Large (full streamed session) |
| Canonical use | Extending an agent with tools | Swapping in a full agent |

### Why each protocol won the use case it did

MCP won tool-extension because:
- Tool calls are bounded: one request, one response
- Agents need many tools; standardizing the tool interface is high-leverage
- Anthropic controlled the spec and shipped Claude with MCP client from day one

ACP won editor-integration because:
- Editors need streaming UX (live thinking, live tool calls, syntax highlighting of agent output)
- Editors need session lifecycle (fork, list, load, authenticate)
- Zed introduced it and the open-source editor ecosystem adopted it

## Why Codex uses MCP to launch sub-agents (including Claude Code)

OpenAI's Codex CLI can consume MCP servers as tools. Anthropic shipped `claude mcp serve` — a mode that exposes Claude Code itself as an MCP server. This makes **Claude Code callable as a tool by Codex**.

### Seven technical reasons MCP beats ACP for sub-agent composition

1. **Agent-as-tool is the cleanest composition primitive.** MCP treats the sub-agent as a callable function. Parent's loop doesn't change — just another tool. ACP would require integrating streaming session lifecycle into the parent's loop.

2. **Context isolation.** If Codex spawns Claude Code to fix a bug, Claude Code internally runs 50 tool calls. With MCP, all that stays inside Claude Code's context — only the final result lands in Codex's context. With ACP, every internal tool call would stream back. For a 20-turn Codex session × 3 sub-agent spawns, MCP gives ~5 kB of sub-agent telemetry; ACP gives ~50 kB. **~10× context saving.**

3. **Request-response matches spawn semantics.** `tools/call` in MCP is request-response — maps cleanly to `future.await`. ACP's streaming session is awkward to treat as a future.

4. **Hierarchical composition.** MCP-inside-MCP is natural: sub-agents can themselves have sub-agents arbitrarily deep via the same protocol. ACP doesn't recurse cleanly.

5. **Simpler authorization.** MCP call = "can this tool be called?" (one allow/deny). ACP = "can this agent initialize a session? capabilities granted? session fork allowed?" (more surface).

6. **No interactive-UX baggage.** MCP doesn't carry "streaming thinking" or "tool call live preview" — because sub-agent-orchestration doesn't need those. ACP carries editor-UX concerns that are irrelevant agent-to-agent.

7. **Remote sub-agents work natively.** MCP has HTTP and SSE transports as first-class, alongside stdio. A sub-agent can run on a different host, inside a container, in a different datacenter — invoked via `https://agents.example.com/claude-code/mcp`. ACP is stdio-only in every production implementation; multi-host ACP requires a non-standard transport shim. This is the **decisive architectural gap for multi-tenant and cloud-deployed orchestration**: MCP naturally supports sub-agent pools on remote hosts (centralized Claude-Code-as-a-service, per-tenant Codex instances, GPU-bound agents on dedicated nodes); ACP does not. The earlier "same-computer" constraint on ACP is an MCP advantage, not just a ACP footnote.

### The political reason — Anthropic's deliberate MCP-first positioning

MCP and ACP have asymmetric political positioning for Anthropic:

**MCP**:
- Anthropic's own protocol; Anthropic controls the spec
- Every MCP-using agent is in Anthropic's ecosystem
- Anthropic benefits from more MCP adoption

**ACP**:
- Zed's protocol
- Treats agents as interchangeable (Claude Code, Copilot, OpenClaw, jiuwenclaw, hermes, Cursor agent — all peer)
- An editor can swap Claude Code for a competitor with minimal work — commoditizes the agent layer

An outside reading: Anthropic would prefer orchestrators consume Claude Code via MCP rather than ACP. MCP:
- Keeps Anthropic's protocol dominant in the composition story
- Lets Claude Code stay differentiated as "the best tool" among peer tools rather than "one agent among peer agents"
- Avoids the ACP commoditization dynamic where the orchestrator layer captures the value

Evidence this is real:
- Anthropic shipped `claude mcp serve` — making Claude Code an MCP tool — as the **first-class** sub-agent-exposure mechanism
- Anthropic supports ACP for editor integration but did not push it as the agent-to-agent composition path
- Codex, hermes, and future orchestrators are converging on MCP-for-sub-agents because it's the best technical fit AND the path of least resistance inside Anthropic's protocol ecosystem

### Validation: hermes's ACP-as-chat-backend is awkward

Hermes's `copilot_acp_client.py` (570 LOC) wraps Copilot's ACP server as a chat backend. The docstring: *"Each request starts a short-lived ACP session, sends the formatted conversation as a single prompt, collects text chunks, and converts the result back into the minimal shape Hermes expects from an OpenAI client."*

Hermes is fighting the protocol — forcing ACP's streaming session shape into a request-response mold because request-response is what sub-agent composition actually needs. A more natural design would be: expose Copilot as an MCP tool. One call, one result.

The existence of 570 LOC to make ACP look like MCP is itself evidence that MCP is the right shape for sub-agent composition.

## Octos's protocol gap — current state

| Protocol | Octos | Recommendation |
|---|---|---|
| MCP client | ✓ present (consumes MCP tool servers) | Keep; extend for sub-agent use case |
| MCP server (octos-as-tool) | ✗ absent | **Add** |
| ACP client | ✗ absent | Add later (editor integration) |
| ACP server (octos-as-agent) | ✗ absent | Add later (editor integration) |
| A2A client / server | ✗ absent | Deferred unless enterprise demand |

## Recommended octos protocol roadmap

### Priority 1 — MCP sub-agent adapter (consume agents as tools)

**Rationale**: octos as orchestrator. Claude Code, Codex, hermes, jiuwenclaw all expose (or will expose) themselves as MCP servers. Octos should be able to dispatch a contract to `claude_code_tool(task="...")` and receive a result. Fits the swarm-future thesis from `OCTOS_SWARM_POSITIONING_2026-04-22.md`.

**Scope**:
- New Rust module `crates/octos-agent/src/tools/mcp_agent.rs`
- `SpawnTool` gains an `agent_mcp` backend: spawn configured MCP agent binary (e.g., `claude mcp serve`, `codex mcp serve`), speak MCP to it, treat it as a callable
- Route task I/O through octos's workspace contract → `MessageTool` semantics
- Sub-agent's internal state stays isolated; only declared artifacts surface
- Reuse M4.1A harness events (`octos.harness.event.v1`) for progress observability
- Reuse M4.3 validator runner for output gating
- Supports hierarchical composition (sub-agent spawns its own sub-agents)

**Estimate**: ~1 kLOC Rust. ~1 engineer-month including tests.

**Deliverable**: one RP-family-style contract-bound issue. Working name: **`#OCTOS_MCP_AGENT` — Call MCP-exposed agents (Claude Code / Codex / etc.) as contract-bound sub-agents**.

### Priority 2 — MCP server mode (expose octos as tool)

**Rationale**: symmetric to priority 1. Once octos can orchestrate, it should also be orchestrable. Lets outer orchestrators (another octos instance, Codex, Claude Code, jiuwenclaw) use octos as a sub-agent. Fits swarm-of-swarms topology.

**Scope**:
- New Rust module `crates/octos-agent/src/mcp_server.rs` (naming TBD)
- Expose octos's existing tool registry as MCP tools, subject to `ToolPolicy` allow lists
- Or expose entire octos sessions as MCP "task" tools: outer MCP client invokes `octos_task(contract=...)` → octos runs the session → returns workspace-contract-gated result
- Preferred: **session-level MCP tool**, not tool-by-tool MCP (keeps isolation semantics of MCP-for-sub-agents)
- Authentication: MCP transport options (stdio parent-trust, HTTP with bearer token)

**Estimate**: ~1.5 kLOC Rust. 1-1.5 engineer-months.

**Deliverable**: contract-bound issue. Working name: **`#OCTOS_MCP_SERVER` — Expose octos sessions via MCP**.

### Priority 3 — ACP client (editor and desktop integration)

**Rationale**: octos-as-desktop-agent for developers using ACP-native editors (Zed, etc.). Less leverage than MCP paths but needed for the developer-workstation use case.

**Scope**:
- New Rust module `crates/octos-agent/src/acp_client.rs`
- Speak ACP over stdio; connect to editor or outer-ACP client
- Map ACP session lifecycle to octos Session
- Stream M4.1A harness events as ACP progress frames

**Estimate**: ~2 kLOC Rust. 1.5-2 engineer-months. (ACP's streaming semantics are richer than MCP's request-response.)

**Deliverable**: optional, after Priority 1 and 2 ship.

### Priority 4 — ACP server (octos-as-agent-to-editor)

**Rationale**: only if octos pursues a Claude-Code-style developer-workstation product. Editor connects to octos via ACP, octos serves coding-agent capability. Predicated on a CLI TUI product posture which is currently **not** octos's direction.

**Estimate**: ~3 kLOC Rust, ~2 engineer-months. Only ship if the product bet is coding-first.

### Priority 5 — A2A

**Deferred**. No current demand signal. OpenJiuwen is the only platform with a real A2A implementation. Revisit if Chinese enterprise agent market becomes a target.

## Why this sequencing beats ACP-first

The earlier internal analysis suggested ACP-client first. **Revised posture**: MCP-agent-tool first.

| Criterion | MCP-agent-tool (P1) | ACP-client (P3) |
|---|---|---|
| Complexity | Simple — agent is one tool | Streaming session lifecycle |
| Context overhead in parent | Small | Large |
| Fits octos's existing infra | Directly — already has MCP client + ToolRegistry | Needs new session-mapping layer |
| Fits swarm topology | Yes — agents-as-tools composes hierarchically | Awkward — ACP-inside-ACP doesn't recurse |
| Effort | ~1 kLOC | ~2 kLOC |
| Consistency with industry direction | Codex / Claude Code both ship MCP sub-agent support | Editor-side only |
| Anthropic relationship | Aligned — uses Anthropic's protocol | Forces ACP commoditization on Anthropic |
| Competitive positioning | Octos orchestrates Claude Code / Codex as sub-agents | Octos peers with Claude Code in Zed's editor |

MCP-agent-tool is strictly more leveraged. Octos becomes a **super-orchestrator** that uses best-in-class coding agents (Claude Code, Codex, hermes) as workers for tasks where their coding-loop depth matters, while octos contributes contract-first task decomposition, workspace validation, multi-channel dispatch, and durable task supervision.

## Competitive positioning after P1+P2 land

**Octos becomes**: the contract-first orchestration platform that can call any MCP-exposed agent as a sub-agent and be called by any MCP host as a sub-agent. Lives above the agent-layer commodity, differentiated by contracts + validators + multi-channel + platform substrate.

**Octos does not become**: a hermes-class coding-loop specialist. That's fine; octos orchestrates hermes-class agents when coding depth matters. Division of labor: octos decomposes and validates; Claude Code / hermes / Codex does the coding.

## Open questions for the team

1. Does the team want **session-level** MCP tool exposure (one MCP tool = one full octos session) or **tool-level** MCP exposure (each of octos's ~20 tools becomes an MCP tool)? Session-level is the swarm-friendly choice; tool-level is legacy agent-as-toolkit.
2. For `OCTOS_MCP_AGENT`, should octos bundle known-good MCP agent configs (Claude Code, Codex) as starter templates, or require operators to configure them?
3. Is HTTP MCP transport needed, or is stdio-only sufficient for the first pass?
4. Should the ACP client path ever be prioritized, or is it perpetually P3? (Ties to the CLI-TUI question in `OCTOS_SWARM_POSITIONING_2026-04-22.md`.)

## Next review

Revisit after Priority 1 ships. The strategic claim — "MCP-for-sub-agents is the path forward" — should be validated by real orchestration against Claude Code and/or Codex with observable token savings and swarm-topology success.
