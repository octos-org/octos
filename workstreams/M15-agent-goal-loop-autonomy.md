# M15 Agent, Goal, And Loop Autonomy Contract

Status: draft contract for backend orchestration execution
Date: 2026-05-15

## Goal

Add the three higher-level coding autonomy features needed to match Codex and
Claude Code behavior without moving orchestration into clients:

1. Codex-style supervised agent lifecycle control.
2. Codex-style persisted `/goal` runtime.
3. Claude Code-style `/loop` fixed-interval, self-paced, and maintenance loops.

M15 sits above M13 and M14. M13 exposes task/artifact inspection. M14 exposes
Codex-compatible model-visible coding tools. M15 defines the backend runtime
that makes long-running project work, supervised child agents, persisted goals,
and recurring prompts coherent.

## Next Milestone: Real AgentOrchestrator

Replace any M15 AppUI in-memory stub state with a real backend-owned
`AgentOrchestrator`. AppUI may expose inspection, hydration, and explicit user
controls, but it must never be the source of truth for agent lifecycle,
scheduling, backend selection, policy inheritance, artifact authorization, or
notification ordering.

The `AgentOrchestrator` is the single server-side control plane for:

- native subagents: in-process `octos_agent::Agent` children created through the
  same session/runtime factory as the parent session.
- CLI agents: subprocess-backed agents with durable lifecycle, stdout/stderr
  capture, interrupt/close semantics, and policy-filtered environment.
- MCP agents: stdio or HTTP MCP-backed agents whose internal tool calls remain
  behind the backend boundary and whose exposed state is normalized into the
  same agent/task/artifact model.

The milestone is a design and contract milestone until implementation begins.
No production runtime code should be introduced by this contract task.

### Contract Invariants

- The backend owns the agent tree. AppUI never creates synthetic agent ids,
  parent links, statuses, artifacts, policy stamps, or ledgers.
- Every child agent is spawned through `AgentOrchestrator::spawn_agent`, even
  when the request originated from `SpawnTool`, `DelegateTool`, a loop fire, a
  goal continuation, or a future swarm planner.
- Every backend implementation returns the same lifecycle vocabulary:
  `pending`, `running`, `waiting`, `completed`, `interrupted`, `failed`,
  `closed`.
- Native, CLI, and MCP agents inherit effective profile, cwd, memory scope,
  tool registry, skills, MCP servers, sandbox, approval policy, model routing,
  QoE limits, and workspace contract policy from the server runtime factory.
- AppUI methods read durable orchestrator state and emit orchestrator
  transitions. Reconnect hydration must not depend on transient client memory.
- Authorization is checked by the orchestrator for control and artifact APIs.
  AppUI clients may request `agent/interrupt` or `agent/close`, but cannot
  bypass session ownership or ancestor-session checks.
- Agent artifacts are exposed through orchestrator-authorized handles and M13
  task/artifact inspection. Raw CLI logs, MCP transport frames, and internal
  tool calls are not leaked into parent chat context unless the backend emits an
  explicit summary or artifact.
- Notification order is monotonic per `(session_id, agent_id)` and terminal
  states are not emitted before the intermediate state that caused them.
- Closing a parent agent closes or tombstones descendants using backend-specific
  cleanup, then emits durable descendant transitions.

### Acceptance Tests

Add focused tests before or alongside implementation:

- Contract test: the M15 docs continue to require `AgentOrchestrator`, native
  subagents, CLI agents, MCP agents, AppUI stub replacement, non-goals, and soak
  evidence.
- Unit test: `AgentOrchestrator` registers a native child with parent path,
  backend kind, runtime policy stamp, and task id.
- Unit test: CLI backend redacts denied env vars, receives sandbox/approval
  policy, and records stdout/stderr as authorized artifacts.
- Unit test: MCP backend normalizes stdio and HTTP failures into structured
  agent status and typed AppUI errors without exposing transport secrets.
- Unit test: `agent/interrupt` and `agent/close` reject non-owner and
  non-ancestor sessions with `agent_control_forbidden`.
- Unit test: closing a parent emits descendant `closed` transitions and prevents
  future artifact reads except for already-authorized immutable artifacts.
- Protocol test: WebSocket and stdio `agent/list`, `agent/status/read`,
  `agent/output/read`, and artifact APIs return identical shapes from the real
  orchestrator store.
- Reconnect test: restarting or reconnecting hydrates the agent tree from
  durable orchestrator/task state, not from in-memory AppUI fixtures.
- Negative test: client-supplied bogus agent ids, policy stamps, backend kinds,
  or parent paths are ignored or rejected and never become effective state.

### Live Soak Evidence Plan

The live soak must run after native, CLI, and MCP backends all use the real
orchestrator path. It should capture both stdio and WebSocket AppUI sessions.

Scenario:

1. Start a coding session and capture advertised `coding.autonomy.v1` and
   `coding.agent_control.v1`.
2. Spawn one native subagent, one CLI-backed agent, and one MCP-backed agent
   through backend-owned model/tool paths.
3. Verify AppUI observes all three in `agent/list` with backend kind, parent
   path, task id, and runtime policy stamp.
4. Interrupt the CLI-backed agent and close the MCP-backed agent from AppUI
   controls.
5. Close the parent or root supervised agent and verify descendants transition
   deterministically.
6. Reconnect over the same transport and hydrate agent, task, and artifact
   state.
7. Repeat the same scenario over the other transport and compare payload
   method names, capability names, error codes, backend kinds, and status
   transitions.

Evidence bundle:

- `appui-transcript.jsonl`
- `server.log`
- `runtime-policy-stamp.json`
- `agent-orchestrator-ledger.jsonl`
- `agent-ledger.jsonl`
- `task-ledger.jsonl`
- `artifact-index.json`
- `native-agent-transcript.jsonl`
- `cli-agent-transcript.jsonl`
- `mcp-agent-transcript.jsonl`
- `transport-parity-report.json`

`agent-orchestrator-ledger.jsonl` schema:
`ts_ms`, `session_id`, `agent_id`, `parent_agent_id`, `backend_kind`,
`event`, `status`, `actor`, `policy_id`, `task_id`, `artifact_count`,
`recoverable`, `reason`.

## Ground Truth

- AppUI change request:
  `docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_021_AGENT_GOAL_LOOP_AUTONOMY.md`
- Supervised task inspection:
  `workstreams/M13-appui-supervised-task-swarms.md`
- Codex-compatible model tools:
  `workstreams/M14-codex-compatible-coding-toolset.md`

## Non-Goals

- Do not make TUI or web clients spawn normal subagents.
- Do not let clients construct agent roles, prompts, tool registries, MCP
  servers, model routing, memory scope, sandbox, or approval policy.
- Do not implement `/loop` as a TUI timer. Scheduling is backend-owned.
- Do not implement `/goal` as a sticky UI checklist. Goal continuation is
  backend-owned and evidence-driven.
- Do not replace M13 `task/*` inspection or M14 model tool aliases.
- Do not keep AppUI-only in-memory agent stubs as a compatibility path once
  `AgentOrchestrator` is available.
- Do not expose generic AppUI `agent/spawn` for normal scheduling; model tools
  and backend runtimes request orchestration through server-owned paths.
- Do not let CLI or MCP backend integrations bypass sandbox, approval,
  workspace, profile, memory, or tool policy by treating them as opaque shells.
- Do not leak raw MCP frames, subprocess environment, secrets, or full child
  transcripts into parent chat state as the inspection contract.

## Tracking Issues

Milestone status is tracked in GitHub and in this table. "Partial" means code
or fixture evidence exists, but the production wiring is not sufficient to
claim Codex-style multi-agent parity.

Central tracker: [octos#992](https://github.com/octos-org/octos/issues/992).

| Milestone | Repo | Issue | Status | Current wiring status |
| --- | --- | --- | --- | --- |
| M15-A: AppUI autonomy protocol | `octos` | [#990](https://github.com/octos-org/octos/issues/990) | Open | Partial: method/capability constants exist; typed durable agent/goal/loop notifications and complete fixtures are not done. |
| M15-B: Backend AgentOrchestrator runtime | `octos` | [#991](https://github.com/octos-org/octos/issues/991) | Open | Partial: in-memory inspection state and fixture CLI agents exist; production native/CLI/MCP child-agent runtime is not wired. |
| M15-C2/C3: Goal scheduler and policy | `octos` | [#979](https://github.com/octos-org/octos/issues/979) | Open | Partial: goal CRUD/status exists; idle continuation, budget policy, and model wrap-up are not wired. |
| M15-D2/D3: Loop scheduler and policy | `octos` | [#977](https://github.com/octos-org/octos/issues/977) | Open | Partial: loop CRUD/control exists; loop fires enqueue only and do not execute production master turns. |
| M15-G1/G2/G3/G4: Master continuation and scatter-join summaries | `octos` | [#976](https://github.com/octos-org/octos/issues/976) | Open | Partial: scheduler primitive exists; no production consumer wakes the master LLM or generates child/final summaries. |
| M15-H: Durable supervisor runtime | `octos` | [#978](https://github.com/octos-org/octos/issues/978) | Open | Partial: `SupervisorStore` exists; it is not wired into session/orchestrator state or restart recovery. |
| M15-E: TUI autonomy UX | `octos-tui` | [#47](https://github.com/octos-org/octos-tui/issues/47) | Open | Partial: slash commands and projection state exist; must be validated against the typed production backend surface. |
| M15-F5: Production autonomy live tmux soak | `octos-tui` | [#44](https://github.com/octos-org/octos-tui/issues/44) | Open | Partial: fixture and real-stdio evidence exists; production non-fixture continuations/goal/loop/restart parity are not proven. |

Dependency order:

- M15-A must land protocol fixtures first.
- M15-B, M15-C, and M15-D consume those fixtures and must not invent divergent
  payload shapes.
- M15-E consumes the M15-A fixtures and hides controls unless the backend
  advertises `coding.autonomy.v1`.
- M15-F1/F2/F3 run only after backend and TUI fixtures agree.
- M15-F4 runs against the same live binaries as F1/F2/F3 and must fail on
  visible UX regressions, not only protocol regressions.

## Workstreams

### M15-A: AppUI Autonomy Protocol

Repository: `octos`

Owns:

- protocol constants, params, results, notifications, and fixtures
- capability negotiation
- spec and UPCR updates

Allowed areas:

- `api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md`
- `docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_021_AGENT_GOAL_LOOP_AUTONOMY.md`
- `crates/octos-core/src/ui_protocol.rs`
- `crates/octos-cli/src/api/ui_protocol.rs`
- protocol tests and JSON fixtures

Deliverables:

- Add capability `coding.autonomy.v1`.
- Add optional capabilities:
  `coding.agent_control.v1`, `coding.goal_runtime.v1`,
  `coding.loop_runtime.v1`.
- Add AppUI methods and notifications for `agent/*`, `session/goal/*`, and
  `loop/*` as defined by UPCR-2026-021.
- Extend `session/status/read.runtime_policy_stamp` with autonomy runtime
  availability, limits, quotas, idle scheduling policy, loop slash-command
  policy, goal budget policy, and continuation rate policy.
- Add typed errors for missing agent, denied artifact, invalid goal state,
  invalid loop interval, empty loop prompt, and policy-denied loop creation.
- Add typed errors for unauthorized agent controls, busy loop fires, denied
  loop slash commands, rate-limited goals, and autonomy quota exhaustion.
- Add JSON fixtures before backend/TUI implementation begins.

Acceptance:

- Unit tests prove all new method params/results round trip.
- Capability-gated methods reject cleanly when not negotiated.
- WebSocket and stdio expose identical payloads.
- Old clients continue to work with all new fields omitted.

### M15-B: Backend AgentOrchestrator Runtime

Repository: `octos`

Owns:

- unified backend `AgentOrchestrator`
- agent registry/tree
- backend implementations for builtin, MCP, and CLI-backed agents
- model-visible lifecycle tools from M14

Allowed areas:

- `crates/octos-agent/src/agent_control/*`
- `crates/octos-agent/src/tools/spawn.rs`
- `crates/octos-agent/src/tools/delegate.rs`
- `crates/octos-agent/src/tools/mcp_agent.rs`
- `crates/octos-agent/src/task_supervisor.rs`
- `crates/octos-swarm/*`
- runtime/session factory modules

Deliverables:

- Introduce a backend-owned `AgentOrchestrator` with:
  `spawn_agent`, `list_agents`, `send_input`, `wait_agent`,
  `interrupt_agent`, `close_agent`, `resume_agent`,
  `artifact_list`, and `artifact_read`.
- Introduce an `AgentBackend` trait with native builtin, lifecycle-capable MCP,
  and CLI process backends.
- Replace M15 AppUI in-memory agent stubs with orchestrator-backed durable
  state. AppUI handlers become projection/control adapters over
  `AgentOrchestrator`; they must not synthesize backend state.
- Adapt existing `SpawnTool`, `DelegateTool`, `TaskSupervisor`, and
  `octos-swarm` to register agent lifecycle state.
- Ensure every child agent is created through the server runtime factory and
  inherits profile, cwd, memory, tools, skills, MCP, sandbox, approval, model,
  QoE, and workspace contract policy.

Acceptance:

- Test: child agents appear in `agent/list` with parent path and policy stamp.
- Test: closing a parent closes descendants.
- Test: interrupt pauses running backend work and updates AppUI.
- Test: MCP and CLI backends cannot bypass sandbox, env, approval, or profile
  policy.
- Test: artifacts are readable only through authorized parent/child sessions.
- Test: `agent/interrupt` and `agent/close` reject non-owner sessions with
  `agent_control_forbidden`.
- Test: native, CLI, and MCP backends all produce the same AppUI lifecycle
  vocabulary and hydrate after reconnect.
- Test: AppUI-provided bogus agent metadata is rejected and never mutates the
  orchestrator store.

Boundary with M14:

- M15-B owns backend lifecycle state, authorization, and child-agent registry.
- M14 owns model-visible tool wrappers that call this runtime.

### M15-C: Backend GoalRuntime

Repository: `octos`

Owns:

- persisted per-session goals
- model-visible goal tools
- idle continuation turns
- budget/time/token accounting

Allowed areas:

- `crates/octos-agent/src/goals/*`
- `crates/octos-agent/src/tools/*`
- `crates/octos-cli/src/api/ui_protocol.rs`
- state/runtime persistence modules
- session actor and runtime factory modules

Deliverables:

- Add persisted goal state:
  `goal_id`, `session_id`, `objective`, `status`, `token_budget`,
  `tokens_used`, `time_used_seconds`, timestamps.
- Add model-visible tools:
  `get_goal`, `create_goal`, and `update_goal`.
- Restrict `update_goal` so the model can only mark `complete`.
- Implement idle continuation:
  active goal, idle session, no pending user input, no approval, no
  request-user-input, no active turn.
- Enforce continuation policy:
  minimum delay between continuations, maximum continuations per wall-clock
  window, and priority behind loop fires.
- Pause active goal on interrupt.
- Reactivate paused goal on resume when policy allows.
- Mark budget-limited goals and inject wrap-up steering.

Acceptance:

- Test: setting a goal persists and emits `session/goal/updated`.
- Test: idle continuation starts another backend turn.
- Test: continuation does not fire while a user decision is pending.
- Test: continuation does not race a due loop fire.
- Test: continuation rate limits emit `goal_rate_limited`.
- Test: interrupt pauses active goal and accounts usage.
- Test: only the backend/user can pause or resume; model can only complete.
- Test: `session/goal/updated` includes `transition_actor`.

### M15-D: Backend LoopRuntime And `/loop`

Repository: `octos`

Owns:

- Claude Code-style `/loop`
- fixed interval scheduling
- self-paced scheduling
- maintenance prompt selection
- loop persistence and max-age expiry

Allowed areas:

- `crates/octos-bus/src/cron_service.rs`
- `crates/octos-bus/src/cron_types.rs`
- `crates/octos-cli/src/cron_tool.rs`
- `crates/octos-cli/src/session_actor.rs`
- `crates/octos-cli/src/api/ui_protocol.rs`
- runtime/session factory modules
- slash command parsing modules

Deliverables:

- Add `/loop [interval] <prompt>` parsing:
  leading interval, trailing `every <interval>`, prompt-only self-paced, and
  bare maintenance loop.
- Add a parser fixture table covering ambiguous inputs, including
  `/loop 5m /foo`, `/loop check 5m logs`, `/loop check deploy every 20m`, and
  invalid dual-interval forms.
- Reuse existing `CronService` for fixed interval loops.
- Add `LoopRuntime` for self-paced loops where the model chooses next delay or
  stop after each iteration.
- Add prompt lookup for bare `/loop`:
  `.octos/loop.md`, then `~/.octos/loop.md`, then built-in fallback.
- Execute the parsed prompt immediately after loop creation.
- Fire loop prompts only while the session is idle.
- Implement `loop/fire_now` as an enqueue request that still respects pause
  state, idle gating, slash-command approval, and runtime policy.
- Persist loop state in the backend state directory. On restart, reload loops,
  skip bulk replay of missed fires, and fire at most once when a due loop
  becomes open and idle.
- Re-resolve bare maintenance loop prompts at fire time.
- Re-authorize slash commands inside loop prompts at every fire.
- Auto-expire recurring loops after configured max age unless trusted backend
  policy marks them permanent.

Acceptance:

- Test: `/loop 5m /foo` schedules a fixed loop and immediately executes `/foo`.
- Test: `/loop check deploy every 20m` parses trailing interval correctly.
- Test: `/loop check deploy` creates a self-paced loop.
- Test: bare `/loop` loads project, user, then fallback prompt in order.
- Test: loop fires wait for idle session and do not interrupt active turns.
- Test: `loop/fire_now` does not bypass idle gating or pause state.
- Test: slash commands inside loops are denied when policy disables them.
- Test: restart reloads loops and does not replay missed fires in bulk.
- Test: changing `.octos/loop.md` changes the next maintenance fire.
- Test: deleting a loop prevents future fires.

### M15-E: TUI Autonomy UX

Repository: `octos-tui`

Owns:

- `/goal` and `/loop` command UX
- autonomy status menus
- agent tree inspection
- old-server fallback

Allowed areas:

- `src/model.rs`
- `src/store.rs`
- `src/transport.rs`
- `src/menu/*`
- command parsing and composer modules
- rendering modules
- TUI docs and tests

Deliverables:

- Add protocol models for M15 AppUI methods and notifications.
- Add `/goal`, `/goal pause`, `/goal resume`, `/goal clear`.
- Add `/loop`, `/loop list`, `/loop delete`, `/loop pause`, `/loop resume`,
  and `/loop fire-now`.
- Render goal and loop status in compact footer/menu surfaces without blocking
  chat content or composer.
- Render agent tree from `agent/list` and `agent/updated`.
- Keep all activity and final summaries in chat history.
- Hide M15 menus when capabilities are missing.
- Add deterministic rendering hooks or debug markers needed by tmux UX
  validation without exposing backend orchestration to the TUI.

Acceptance:

- Test: old server hides goal/loop/agent controls.
- Test: `/goal` and `/loop` commands call AppUI, not local timers.
- Test: `/loop fire-now` displays queued/busy/denied server states instead of
  pretending to fire locally.
- Test: agent, goal, and loop updates hydrate after reconnect.
- Test: no sticky pane covers the composer or bottom chat line.
- Test: tmux text selection remains usable.
- Test: markdown headings, lists, tables, bold, and code blocks render in chat
  messages, agent summaries, and tool/activity summaries.
- Test: composer supports paste, multiline resize, shortcut editing, and CJK
  cursor placement at the visual end of the input.
- Test: activity groups are interleaved under the related chat turn and are not
  stranded in a permanent bottom pane.
- Test: progress state has one visible spinner/state indicator, not duplicated
  spinner text in both chat and sticky/header regions.
- Test: menu search filters `/model`, `/mcp`, `/tools`, `/goal`, and `/loop`
  entries using the same input field and preserves selection after filtering.

### M15-F1: Live Stdio Autonomy Soak

Repository: `octos` and `octos-tui`

Owns:

- tmux interactive e2e fixture for stdio
- evidence bundle for agent, goal, and loop autonomy over stdio

Deliverables:

- Stdio live tmux soak covering the shared scenario below.
- Stdio transcript assertions for capability negotiation, notification order,
  policy stamps, loop fire gating, and reconnect hydration.

Acceptance:

- Stdio proves TUI never spawns agents or runs loop timers locally.
- Stdio transcript shows monotonic `updated_at_ms` per agent, goal, and loop.

### M15-F2: Live WebSocket Autonomy Soak

Repository: `octos` and `octos-tui`

Owns:

- tmux interactive e2e fixture for WebSocket
- WebSocket parity with stdio payloads

Deliverables:

- WebSocket live tmux soak covering the shared scenario below.
- Diff tool comparing WebSocket and stdio transcript method names, capability
  names, error codes, and state transitions.

Acceptance:

- WebSocket and stdio expose identical payload shapes.
- WebSocket notification ordering matches the per-entity ordering contract.

### M15-F3: Reconnect And Hydration Autonomy Soak

Repository: `octos` and `octos-tui`

Owns:

- tmux interactive e2e fixtures
- reconnect/hydration assertions for agent, goal, loop, task, and artifact state

Allowed areas:

- `e2e/*`
- `scripts/*`
- TUI e2e harness files
- backend test fixtures

Deliverables:

Shared scenario:
  1. Open coding session.
  2. Refresh capabilities and status stamp.
  3. Start a goal and verify continuation.
  4. Pause/resume/clear the goal.
  5. Create fixed `/loop 1m` and verify immediate fire.
  6. Create self-paced `/loop prompt` and verify next-run state.
  7. Trigger backend-owned review that spawns child agents.
  8. Interrupt and close one child agent.
  9. Reconnect and hydrate agent, goal, loop, task, and artifact state.
  10. Attach a second client and verify unauthorized control attempts fail.

Evidence bundle:

- `appui-transcript.jsonl`
- `server.log`
- `runtime-policy-stamp.json`
- `agent-ledger.jsonl`
- `goal-ledger.jsonl`
- `loop-ledger.jsonl`
- `task-ledger.jsonl`
- `artifact-index.json`
- `tui-capture.txt`

Evidence ledger schemas:

- `agent-ledger.jsonl`: `ts_ms`, `session_id`, `agent_id`,
  `parent_agent_id`, `event`, `status`, `actor`, `policy_id`, `task_id`.
- `goal-ledger.jsonl`: `ts_ms`, `session_id`, `goal_id`, `event`, `status`,
  `transition_actor`, `tokens_used`, `token_budget`, `reason`.
- `loop-ledger.jsonl`: `ts_ms`, `session_id`, `loop_id`, `event`, `status`,
  `mode`, `interval_seconds`, `next_run_at_ms`, `actor`, `reason`.
- `task-ledger.jsonl`: `ts_ms`, `session_id`, `task_id`, `event`, `status`,
  `agent_id`, `turn_id`.

Acceptance:

- Reconnect proves deltas are reconciled from durable `agent/status/read`,
  `agent/list`, and M13 `task/*` state.
- Soak proves all scheduling decisions are visible in AppUI transcript.
- Soak proves final chat lines and composer are not obscured.

### M15-F4: Live Tmux UX Feature Soak Validation

Repository: `octos-tui`

Owns:

- terminal UX validation fixture
- tmux input replay scripts
- screen capture and layout assertions
- regression checks for the visible UX bugs reported during manual testing

Allowed areas:

- `e2e/*`
- `scripts/*`
- TUI e2e harness files
- rendering, composer, menu, markdown, scroll, and activity grouping tests

Deliverables:

- Add a live tmux UX soak that drives a real `octos-tui` session with scripted
  keystrokes and paste events.
- Capture terminal frames after each step using tmux capture-pane with stable
  dimensions: narrow, standard, and tall layouts.
- Emit machine-readable `ux-validation.json` with pass/fail checks, terminal
  dimensions, cursor position, visible panes, and failing frame excerpts.
- Save raw artifacts:
  `tui-capture.txt`, `tui-capture-before-scroll.txt`,
  `tui-capture-after-scroll.txt`, `composer-capture.txt`,
  `menu-capture.txt`, `diff-preview-capture.txt`, `input-replay.log`,
  `terminal-size.json`.
- Provide a validator that fails if a known bug pattern appears in the captured
  terminal text or layout geometry.

Required validation cases:

1. Open onboarding or an empty coding session and verify only one primary
   composer is active.
2. Paste a multiline prompt and verify the composer grows without hiding the
   final chat line.
3. Type Chinese text and verify the cursor column is at the visual end of the
   input after each character and after paste.
4. Send a prompt that produces markdown headings, bullets, numbered lists,
   tables, bold text, and fenced code. Verify rendered output does not expose
   raw markdown markers where rich rendering is expected.
5. Trigger a tool-heavy coding task. Verify tool/activity summaries are grouped
   under the related assistant turn, indented, and remain in chat history.
6. Verify there is no permanent task/turn/sticky plan pane covering the bottom
   chat region. Turn plans and follow-up questions must render as chat content.
7. Verify progress state appears in exactly one place while a turn is running.
8. Verify diff preview appears as an explicit chat/activity item or selectable
   inspector, not as a bottom composer overlay.
9. Open `/model`, `/mcp`, `/tools`, `/goal`, and `/loop` menus. Search for
   entries and verify filtering, highlighting, and selection work.
10. Scroll up and down through a long run. Verify no lock-up, no duplicated
    scroll jumps, no permanent hidden bottom line, and tmux text selection still
    captures normal chat text.
11. Reconnect and verify hydrated agent/goal/loop state does not reintroduce
    sticky panes or duplicate completed task summaries.

Acceptance:

- Fails when the bottom chat line is covered by any footer, sticky pane, diff
  preview, or composer overlay.
- Fails when an activity group for a completed turn remains outside chat
  history after the turn completes.
- Fails when markdown tables, headings, bullets, or code fences visibly render
  as broken plain text in assistant responses.
- Fails when CJK input changes the cursor position away from the visual end.
- Fails when paste drops lines, hides lines, or leaves the composer height stale.
- Fails when menu search does not filter or loses selection.
- Fails when more than one spinner/progress indicator is visible for one turn.
- Fails when tmux text selection is blocked by alternate-screen or mouse-mode
  settings after the soak completes.
- Produces `ux-validation.json` and raw capture files for every run.
