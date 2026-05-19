# UPCR-2026-021: Agent, Goal, And Loop Autonomy Inspection

Status: proposed
Date: 2026-05-15

## Summary

Add AppUI inspection and user-control surfaces for three backend-owned coding
autonomy features:

1. Codex-style supervised agent lifecycle.
2. Codex-style persisted thread goals.
3. Claude Code-style `/loop` scheduled or self-paced recurring prompts.

These features are backend runtime features. TUI and web clients render server
truth, send explicit user controls, and never construct model tools, child
agents, goal continuations, loop prompts, model routing, sandbox policy, memory,
MCP servers, or tool registries locally.

The next backend milestone replaces M15 AppUI in-memory agent stubs with a real
server-owned `AgentOrchestrator`. AppUI methods in this UPCR are projection and
control surfaces over orchestrator state; they are not an agent scheduler,
agent registry, artifact authority, or transport-specific cache.

## Relationship To Existing Contracts

- M13 owns supervised task and artifact inspection through `task/*`.
- M14 owns Codex-compatible model-visible coding tools.
- M15 owns the higher-level autonomy runtime that coordinates agent lifecycle,
  persisted goals, and recurring loop prompts.
- M15-B owns the `AgentOrchestrator` contract that normalizes native subagents,
  CLI agents, and MCP agents into the AppUI agent lifecycle surface.
- `agent/output/read` and `agent/output/delta` are compact status/message-tail
  surfaces only. Full transcripts, tool calls, and durable artifacts remain
  under M13 `task/*` and artifact APIs.

## Production Milestone Map

The AppUI surface in this UPCR is not complete until the backend runtime below
exists. The current in-process state and M15 live fixture prove protocol shape
and scatter-join evidence only.

Central tracker: https://github.com/octos-org/octos/issues/992

- **M15-A: AppUI autonomy protocol.** Capability gates, method names,
  notifications, typed errors, and runtime policy stamp. Tracking:
  https://github.com/octos-org/octos/issues/990
- **M15-B: Backend AgentControl runtime.** Server-owned agent lifecycle,
  status, output, artifacts, interrupt, and close over native/CLI/MCP agents.
  Tracking: https://github.com/octos-org/octos/issues/991
- **M15-G1: MasterContinuationScheduler.** Durable backend queue that wakes
  the master turn for child completion, scatter-join completion, loop fires,
  and eligible goal continuations. Tracking:
  https://github.com/octos-org/octos/issues/976
- **M15-G2: Child-completion wakeup.** A terminal child event enqueues a master
  continuation with compact child-result context. Prompt history alone is not
  treated as a scheduler.
- **M15-G3: Model-generated child progress summaries.** The resumed master LLM
  produces visible "one child finished" summaries from sanitized child result
  context.
- **M15-G4: Model-generated scatter-join final answer.** After all children
  reach terminal state, the resumed master LLM receives joined artifacts and
  produces the final answer.
- **M15-H: Durable supervisor runtime.** Persist supervised groups, children,
  heartbeats, terminal states, pending continuations, artifacts, and join state
  across restart and reconnect. Tracking:
  https://github.com/octos-org/octos/issues/978
- **M15-C: Backend GoalRuntime API.** Goal CRUD/status, transition actor,
  policy metadata, and AppUI notifications.
- **M15-C2: Goal continuation scheduler.** Active goals continue only when the
  session is idle and after higher-priority user input, approvals, loop fires,
  and child-completion continuations are clear.
- **M15-C3: Goal execution policy.** Enforce token/time budget, pause/resume,
  interruption behavior, model-complete transitions, and budget-exhaustion
  wrap-up. Tracking: https://github.com/octos-org/octos/issues/979
- **M15-D: Backend LoopRuntime API.** Loop CRUD/status, parsing, controls, and
  AppUI notifications.
- **M15-D2: Loop fire scheduler.** Fixed, self-paced, maintenance, and manual
  `fire_now` loops enqueue backend continuations through the same scheduler and
  emit `loop/fired`/`loop/completed`.
- **M15-D3: Loop execution policy.** Resolve maintenance prompts at fire time,
  re-authorize slash commands on every fire, enforce pause/idle/busy policy,
  and persist next-run decisions. Tracking:
  https://github.com/octos-org/octos/issues/977
- **M15-F5: Production autonomy live tmux soak.** TUI/e2e proof for real master
  continuations, goal continuation, loop fires, restart hydration, and
  stdio/WebSocket parity. Tracking:
  https://github.com/octos-org/octos-tui/issues/44

## Capabilities

Required capability for the combined M15 inspection surface:

- `coding.autonomy.v1`

Optional capability groups:

- `coding.agent_control.v1`
- `coding.goal_runtime.v1`
- `coding.loop_runtime.v1`

Clients must gate every method, menu, footer indicator, and hydrate path on the
advertised capability. Missing capabilities must not be filled from local config.

## Agent Lifecycle Surface

Normal subagent scheduling remains model/backend-owned. AppUI exposes
inspection plus user safety controls over already-created agents.

The backing state for every method in this section must come from
`AgentOrchestrator`, not an AppUI-local in-memory stub. The orchestrator owns
agent ids, parent links, backend kind, lifecycle status, policy stamp, artifact
handles, and reconnect hydration.

Supported backend kinds:

- `native`: in-process Octos subagent created by the server runtime factory.
- `cli`: subprocess-backed agent with backend-owned lifecycle and artifact
  capture.
- `mcp`: stdio or HTTP MCP-backed agent normalized into the same lifecycle
  model.

Methods:

- `agent/list`
- `agent/status/read`
- `agent/output/read`
- `agent/artifact/list`
- `agent/artifact/read`
- `agent/interrupt`
- `agent/close`

Notifications:

- `agent/updated`
- `agent/output/delta`
- `agent/artifact/updated`

Typed notification payloads:

- `agent/updated`: `{ "session_id": SessionKey, "agent": Agent }`.
- `agent/output/delta`: `{ "session_id": SessionKey, "agent_id": string,
  "cursor": { "offset": number }, "text": string }`.
- `agent/artifact/updated`: `{ "session_id": SessionKey, "agent_id": string,
  "artifacts": AgentArtifact[] }`.

`AgentArtifact` uses the same compact metadata shape as `agent/artifact/list`:
`id`, `title`, `kind`, `status`, with optional backend-owned fields such as
`path` or `content` when the caller is authorized to see them.

`agent/list` result shape:

```json
{
  "agents": [
    {
      "agent_id": "agent_01",
      "parent_agent_id": "root",
      "session_id": "coding:local:tui#coding",
      "task_id": "task_01",
      "path": "/root/reviewer",
      "role": "reviewer",
      "nickname": "reviewer",
      "backend_kind": "native",
      "status": "running",
      "last_task": "review changed Rust files",
      "cwd": "/repo",
      "profile_id": "coding",
      "runtime_policy_stamp": {
        "profile_id": "coding",
        "sandbox": "workspace-write",
        "approval_policy": "on-request",
        "tool_policy_id": "coding-v1"
      },
      "artifact_count": 2,
      "created_at_ms": 1778870000000,
      "updated_at_ms": 1778870030000
    }
  ]
}
```

Agent status values:

- `pending`
- `running`
- `waiting`
- `completed`
- `interrupted`
- `failed`
- `closed`

Clients may interrupt or close existing agents when the user explicitly asks.
Clients must not expose generic `agent/spawn` as the normal scheduling path.

Control authorization:

- `agent/interrupt` and `agent/close` require the caller to own the session or
  an ancestor session of the target agent.
- Concurrent clients attached to the same session may request controls, but the
  backend serializes state transitions and emits the winning terminal state.
- Unauthorized control attempts fail with `agent_control_forbidden`.
- `agent/output/delta` is best-effort. After reconnect, clients must reconcile
  with `agent/status/read`, `agent/list`, and M13 `task/*` state.
- Client-supplied `agent_id`, `parent_agent_id`, `backend_kind`, or
  `runtime_policy_stamp` values are never accepted as effective state. They may
  only identify a requested control target where the backend already has a
  matching authorized agent.
- CLI and MCP backends must not bypass sandbox, approval, workspace, profile,
  memory, or tool policy by treating the external process or remote transport
  as trusted.

## Goal Runtime Surface

Goal mode persists a long-running objective for a coding thread. The backend
continues the goal only when the session is idle and no user input, approval,
or pending tool/user decision blocks the turn.

Methods:

- `session/goal/get`
- `session/goal/set`
- `session/goal/clear`

Notifications:

- `session/goal/updated`
- `session/goal/cleared`

Typed notification payloads:

- `session/goal/updated`: `{ "session_id": SessionKey, "profile_id"?:
  string, "goal": Goal, "transition_actor": "user" | "backend" | "model" }`.
- `session/goal/cleared`: `{ "session_id": SessionKey, "profile_id"?:
  string, "cleared": boolean, "goal": null, "transition_actor": "user" |
  "backend" | "model" }`.

Goal shape:

```json
{
  "session_id": "coding:local:tui#coding",
  "goal": {
    "goal_id": "goal_01",
    "objective": "finish the review and tests",
    "status": "active",
    "token_budget": 50000,
    "tokens_used": 3200,
    "time_used_seconds": 180,
    "created_at_ms": 1778870000000,
    "updated_at_ms": 1778870030000
  }
}
```

Goal statuses:

- `active`
- `paused`
- `budget_limited`
- `complete`

Rules:

- TUI may set, pause, resume, or clear a goal only from explicit user action.
- The model-visible tool may only mark a goal complete.
- Goal updates must include `transition_actor`: `user`, `backend`, or `model`.
- Interrupting a running turn pauses the active goal.
- Resuming a session may reactivate a paused goal when policy allows it.
- Budget exhaustion marks the goal `budget_limited` and asks the model to wrap
  up, not to start new work.
- Idle continuation is rate limited by backend policy. A session must observe
  the configured minimum delay between goal continuations and the configured
  maximum continuations per wall-clock window.
- Scheduling priority is: user input, approvals/user decisions, loop fire, then
  goal continuation. Goal continuation must never race a loop fire.

## Loop Runtime Surface

Loop mode implements Claude Code-style `/loop`.

Supported forms:

- `/loop 5m /foo` runs `/foo` repeatedly at a fixed interval.
- `/loop 30m check deploy` runs a prompt repeatedly at a fixed interval.
- `/loop check deploy every 20m` runs a prompt repeatedly at a fixed interval.
- `/loop check deploy` lets the model self-pace the next interval.
- `/loop` runs a maintenance prompt from `.octos/loop.md`,
  `~/.octos/loop.md`, or a built-in fallback.

Parsing rules:

- A leading interval is recognized only when the first token is a duration.
- A trailing interval is recognized only from `every <duration>` at the end.
- If both leading and trailing intervals are present, reject with
  `loop_invalid_interval`.
- Without an interval and with a non-empty prompt, create a self-paced loop.
- Without an interval and without a prompt, create a maintenance loop.

Methods:

- `loop/create`
- `loop/list`
- `loop/delete`
- `loop/pause`
- `loop/resume`
- `loop/fire_now`

Notifications:

- `loop/updated`
- `loop/fired`
- `loop/completed`

Typed notification payloads:

- `loop/updated`: `{ "session_id": SessionKey, "profile_id"?: string,
  "loop_id"?: string, "loop": Loop, "ok"?: boolean, "status"?: string,
  "deleted"?: boolean }`.
- `loop/fired`: `{ "session_id": SessionKey, "profile_id"?: string,
  "loop_id": string, "loop"?: Loop, "fire"?: LoopFire, "ok"?: boolean,
  "status"?: string }`.
- `loop/completed`: `{ "session_id": SessionKey, "profile_id"?: string,
  "loop_id": string, "loop"?: Loop, "status"?: string,
  "completed_at_ms"?: number, "result"?: object, "error"?: string }`.

`LoopFire` mirrors the `loop/fire_now` result object: `queued`, optional
`duplicate`, `continuation_id`, `dedupe_key`, `reason`, `priority`, and
`message`.

Loop shape:

```json
{
  "loop_id": "loop_01",
  "session_id": "coding:local:tui#coding",
  "prompt": "check deploy",
  "mode": "self_paced",
  "interval_seconds": null,
  "status": "active",
  "next_run_at_ms": 1778870600000,
  "last_run_at_ms": 1778870000000,
  "expires_at_ms": 1779474800000,
  "created_at_ms": 1778870000000,
  "updated_at_ms": 1778870030000
}
```

Loop modes:

- `fixed_interval`
- `self_paced`
- `maintenance`

Loop statuses:

- `active`
- `paused`
- `running`
- `completed`
- `expired`
- `failed`
- `deleted`

Rules:

- Loop prompts fire only while the session is open and idle.
- Creating a loop must immediately execute the parsed prompt once.
- `loop/fire_now` enqueues a user-requested fire. It still honors pause state,
  idle-only scheduling, slash-command approval policy, and all runtime policy.
  It returns `loop_busy`, `loop_policy_denied`, or `loop_slash_denied` instead
  of bypassing backend scheduling.
- Recurring loops auto-expire after the configured max age unless explicitly
  marked permanent by trusted backend policy.
- Slash commands inside loop prompts are passed through the same backend slash
  command dispatcher as ordinary user input and must be re-authorized at every
  fire, not only at loop creation.
- Self-paced loops require the model to choose the next delay or stop through
  backend-owned loop runtime tools.
- Loops are persisted in the backend state directory. On restart, loops reload
  with their last known state; missed fires are not replayed in bulk. If a loop
  is due when the session becomes open and idle, the backend may fire it once
  and then recompute `next_run_at_ms`.
- Maintenance loop prompts are resolved at fire time, not creation time, so
  updates to `.octos/loop.md` or `~/.octos/loop.md` take effect on the next
  fire.

Notification ordering:

- Notifications are ordered per `(session_id, entity_id)`.
- The backend may coalesce delta notifications, but must not reorder terminal
  state notifications after intermediate state notifications.
- `updated_at_ms` must be monotonic per entity.

## Runtime Policy

`session/status/read.runtime_policy_stamp` must include autonomy state when
`coding.autonomy.v1` is negotiated:

```json
{
  "autonomy_contract_id": "coding-autonomy-v1",
  "agent_control": "available",
  "goal_runtime": "available",
  "loop_runtime": "available",
  "goal_default_token_budget": 50000,
  "goal_max_token_budget": 200000,
  "continuation_min_delay_seconds": 30,
  "continuation_max_per_hour": 20,
  "loop_min_interval_seconds": 60,
  "loop_max_interval_seconds": 86400,
  "loop_max_age_days": 7,
  "loop_allow_slash_commands": true,
  "idle_only_scheduling": true,
  "max_objective_bytes": 8192,
  "max_loop_prompt_bytes": 8192,
  "max_loops_per_session": 16,
  "max_agent_tree_depth": 4,
  "max_agents_per_session": 32
}
```

The stamp must describe effective backend state. It must not echo requested or
locally inferred state.

## Error Model

Add structured error kinds:

- `agent_not_found`
- `agent_control_forbidden`
- `agent_control_unavailable`
- `agent_artifact_denied`
- `goal_runtime_unavailable`
- `goal_unavailable`
- `goal_invalid_state`
- `goal_rate_limited`
- `loop_runtime_unavailable`
- `loop_not_found`
- `loop_invalid_interval`
- `loop_prompt_empty`
- `loop_busy`
- `loop_slash_denied`
- `loop_policy_denied`
- `autonomy_quota_exceeded`

Errors must include `session_id`, relevant entity id, `profile_id`, policy id,
and `recoverable` where applicable. Secrets must be omitted.

## Tests

- Documentation contract test keeps this UPCR and the M15 workstream aligned on
  `AgentOrchestrator`, native subagents, CLI agents, MCP agents, AppUI stub
  replacement, non-goals, and live soak evidence.
- WebSocket and stdio expose identical capabilities, methods, notifications,
  and errors.
- `agent/list` hydrates an existing child agent tree after reconnect.
- `agent/list` and `agent/status/read` read from the backend
  `AgentOrchestrator`, not AppUI-local stub state.
- Native, CLI, and MCP agents produce the same lifecycle status vocabulary and
  typed error model.
- `agent/interrupt` pauses a running agent and updates status.
- `agent/interrupt` and `agent/close` reject unauthorized clients with
  `agent_control_forbidden`.
- M15-G1: pending master continuations persist with reason, priority,
  parent/child ids, context generation, and enqueue/dequeue timestamps.
- M15-G2: when one child reaches terminal state, a master continuation is
  enqueued and dequeued only after the session is idle.
- M15-G3: the master LLM receives sanitized child result context and emits a
  model-generated progress summary for that child.
- M15-G4: after all children reach terminal state, the master LLM emits one
  final joined answer that cites joined artifacts.
- M15-H: restart reloads supervisor groups, child states, pending
  continuations, and artifacts without duplicating completed continuations.
- `session/goal/set` persists an active goal and emits `session/goal/updated`.
- Interrupting a goal turn pauses the goal.
- Idle goal continuation does not fire while user input, approval, or a
  request-user-input decision is pending.
- Goal continuation respects continuation delay and priority behind loop fires.
- M15-C2/C3: an active goal produces a backend-owned continuation after idle
  delay, honors budget limits, and asks the model to wrap up on exhaustion.
- `/loop 5m /foo` creates a fixed loop and immediately fires once.
- `/loop prompt` creates a self-paced loop and records the model-selected next
  run time.
- Bare `/loop` uses `.octos/loop.md`, user loop file, or fallback prompt in
  that priority order.
- `loop/fire_now` does not bypass pause, idle gating, or slash-command policy.
- Restart reloads persisted loops and does not replay missed fires in bulk.
- Slash commands in loop prompts are re-authorized at every fire.
- M15-D2/D3: fixed, self-paced, maintenance, and `fire_now` loop fires enqueue
  master continuations through the same scheduler used by child completion and
  goal continuation.
- Transport soaks assert monotonic entity updates and no notification reorder
  for agent, goal, or loop state.
- Negative test: client-supplied bogus autonomy hints never appear in the
  effective runtime policy stamp.
- Live tmux soak captures AppUI transcript, server log, goal/loop/agent ledgers,
  runtime policy stamp, and TUI capture.

## Live Soak Evidence

The M15-B soak must prove that AppUI is observing a real backend orchestrator.
Run the shared scenario over stdio and WebSocket:

1. Start a coding session and record capabilities plus
   `runtime_policy_stamp`.
2. Spawn one native subagent, one CLI-backed agent, and one MCP-backed agent
   through backend-owned model/tool paths.
3. Observe all three through `agent/list` with backend kind, parent path, task
   id, and policy stamp.
4. Interrupt the CLI-backed agent and close the MCP-backed agent through AppUI
   controls.
5. Reconnect and hydrate the same agent/task/artifact state.
6. Compare stdio and WebSocket transcripts for method names, capability names,
   error codes, backend kinds, and status transitions.

Required evidence bundle:

- `appui-transcript.jsonl`
- `server.log`
- `runtime-policy-stamp.json`
- `agent-orchestrator-ledger.jsonl`
- `agent-ledger.jsonl`
- `agent-ledger.jsonl` must include `agent_started`, periodic `agent_ping`,
  and terminal `agent_completed`/`agent_failed`/`agent_interrupted` rows for
  every supervised child.
- `task-ledger.jsonl`
- `artifact-index.json`
- `native-agent-transcript.jsonl`
- `cli-agent-transcript.jsonl`
- `mcp-agent-transcript.jsonl`
- `transport-parity-report.json`
- `ux-validation.json` proving the TUI rendered per-agent completion updates
  as they arrived and one final scatter-join summary after all children reached
  terminal state.

## Explicit Non-Goals

- Do not implement AppUI-owned agent scheduling or generic `agent/spawn`.
- Do not keep AppUI-only in-memory agent stubs as the compatibility path once
  orchestrator-backed state is available.
- Do not let the TUI or web client construct prompts, roles, tool registries,
  MCP server configuration, sandbox policy, or approval policy for child
  agents.
- Do not expose raw MCP frames, subprocess environment, secrets, or full child
  transcripts as parent chat state.
- Do not replace M13 task/artifact inspection or M14 model-visible tool aliases.
