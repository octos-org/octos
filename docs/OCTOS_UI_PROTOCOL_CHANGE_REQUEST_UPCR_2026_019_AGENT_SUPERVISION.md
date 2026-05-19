# UPCR-2026-019: AppUI Backend-Owned Supervised Task Inspection

Status: proposed
Date: 2026-05-15

## Summary

Expose Octos' backend-owned subagent supervision state through AppUI without
making the TUI a subagent scheduler.

Codex comparison changed this contract: Codex exposes many imperative APIs, but
they mostly represent user/app actions such as `turn/start`, `review/start`,
`turn/interrupt`, config, auth, fs, shell utility, and thread management.
Codex collaborative subagent operations (`spawnAgent`, `sendInput`, `wait`,
`closeAgent`) are model-visible tools surfaced back to UI as thread items and
events. The normal UI does not decide that a code review needs a subagent; the
backend/model does.

Octos should copy that separation:

- TUI sends `turn/start` for normal user messages.
- Optional `/review` or menu review UX may call a typed `review/start`.
- Backend LLM/runtime schedules child work through server-owned tools.
- AppUI projects child task state, summaries, artifacts, policy stamps, and
  reconnect hydration data.

## Decision

Do not add a generic `agent/*` namespace.

Do not make `task/spawn`, `task/send`, or `task/join` part of the required M13
TUI contract. Those are future operator/debug controls at most. They are not
needed for the normal flow where a user asks for code review and the backend
LLM schedules supervised workers.

M13 AppUI is inspection-first:

- extend `task/list`
- extend `task/updated`
- add `task/artifact/list`
- add `task/artifact/read`
- optionally add `review/start` for typed review commands

Existing `task/list`, `task/cancel`, `task/restart_from_node`,
`task/output/read`, `task/updated`, `task/output/delta`, and
`turn/spawn_complete` remain the lifecycle and output surface.

## Capabilities

Servers that support this contract advertise these additive feature flags:

- `harness.task_supervision_inspection.v1`
- `harness.task_artifacts.v1`
- `review.start.v1` when typed review start is implemented

Capability-gated methods must appear in `supported_methods` only when their
gate is negotiated.

## Required Projection

### Backend Supervisor Semantics

Subagent lifecycle is backend-owned. A supervised scatter-join workflow must
track these states for every child task or agent:

- `started`: the child runtime was created by the backend runtime factory.
- `running`: the child is alive and has not produced a terminal result.
- `heartbeat`: the supervisor has observed that the child is still alive.
- `completed`: the child produced its terminal summary and artifacts.
- `failed`: the child exited with an error or violated runtime policy.
- `interrupted`: the parent/user cancelled the child.
- `closed`: the supervisor no longer retains live process resources.

The supervisor must emit live `task/updated` projections for state changes and
periodic heartbeat detail while children are running. When one child completes,
the parent turn should receive an inline completion summary for that child.
When all children have reached a terminal state, the parent turn must receive a
scatter-join summary that includes completed/failed/interrupted counts and
links to artifacts.

For reconnect and audit, the same state must be readable from `task/list`,
`task/output/read`, and `task/artifact/*`; the TUI must not infer child
completion from local timers.

### `task/list`

Gate: existing task-control support plus
`harness.task_supervision_inspection.v1` for the new fields.

Extend each task entry with backend-owned supervision metadata:

- `parent_task_id`
- `parent_session_id`
- `child_session_id`
- `source`: `llm_tool`, `review`, `pipeline`, `operator`, or `unknown`
- `role_id`
- `role_name`
- `requested_by_turn_id`
- `runtime_policy_stamp`
- `artifact_count`
- `summary`
- `join_state`
- `last_heartbeat_at`
- `heartbeat_count`
- `terminal_reason`

Example result:

```json
{
  "session_id": "local:demo",
  "tasks": [
    {
      "id": "019e0000-0000-7000-8000-000000000001",
      "state": "running",
      "source": "llm_tool",
      "parent_task_id": null,
      "parent_session_id": "local:demo",
      "child_session_id": "local:demo#child-1",
      "role_id": "repo-reviewer",
      "role_name": "Repository Reviewer",
      "requested_by_turn_id": "turn-1",
      "artifact_count": 0,
      "summary": "Reviewing authentication and config loading.",
      "join_state": "not_joined",
      "last_heartbeat_at": "2026-05-15T00:00:24Z",
      "heartbeat_count": 3,
      "terminal_reason": null,
      "runtime_policy_stamp": {
        "profile_id": "coding",
        "model": "deepseek-v4-pro",
        "sandbox": "workspace-read",
        "tool_policy_id": "coding-review"
      }
    }
  ]
}
```

### `task/updated`

Gate: existing task event support plus
`harness.task_supervision_inspection.v1` for the new fields.

Carry the same supervision metadata as `task/list` for live updates. This lets
TUI/web render backend-owned subagent activity after reconnect without inventing
local state.

## Optional Typed Product Command

### `review/start`

Gate: `review.start.v1`

Starts the product-level automated review workflow. This is not generic
subagent spawning; it is equivalent to Codex `review/start`.

Params:

```json
{
  "session_id": "local:demo",
  "target": {
    "type": "uncommitted_changes"
  },
  "delivery": "inline"
}
```

Targets:

- `uncommitted_changes`
- `base_branch`
- `commit`
- `custom`

If this method is absent, TUI can still send a plain `turn/start` asking for
review.

Result:

```json
{
  "accepted": true,
  "session_id": "local:demo",
  "turn_id": "turn-2",
  "workflow": "code_review",
  "backend": "native",
  "agent_count": 4
}
```

Runtime behavior:

- the server emits `turn/started` for the returned `turn_id`
- the server launches backend-owned specialists and surfaces lifecycle through
  `agent/updated`, `agent/output/delta`, and `agent/artifact/updated`
- the native specialist list is server-resolved, not hard-coded into AppUI:
  `OCTOS_REVIEW_NATIVE_SPECIALISTS_JSON` may override it for test/operator
  runs; otherwise the active profile's `review.native_specialists` list is
  used; otherwise the server falls back to its built-in default review
  template
- the server also mirrors legacy `TaskSupervisor` background tasks into the
  same agent surface, so model-triggered `spawn_only`, `run_pipeline`, and
  child-session work are visible through `agent/list`, `agent/status/read`,
  `agent/output/read`, and `agent/artifact/*`
- one-child-finished progress is surfaced as normal `message/delta`
- the final joined review answer is model-generated by the master review join
  step, persisted to the session, and followed by `turn/completed`

Implementation note, 2026-05-16:

- `TaskSupervisor` progress forwarding now upserts mirrored background agents
  into the default `AgentOrchestrator`.
- Mirrored terminal transitions emit typed `agent/updated` notifications and
  share the master continuation scheduler used by native specialists.
- Repeated terminal upserts are intentionally de-duplicated so a completed
  child cannot wake the master repeatedly for the same completion.
- Real stdio evidence now covers the ordinary `TaskSupervisor` mirror path:
  `node e2e/scripts/m15-task-supervisor-mirror-stdio-soak.mjs` passed with a
  deterministic task-output fixture and verified `agent/updated`,
  `agent/list`, `agent/status/read`, and `task/output/read`.
  Evidence:
  `/Users/yuechen/home/octos/e2e/test-results-m15-task-supervisor-mirror-stdio/20260516T224154Z`
- Real tmux evidence now covers the same ordinary `TaskSupervisor` mirror path
  through `octos-tui` against real `octos serve --stdio`:
  `e2e/scripts/m15-task-supervisor-mirror-tmux-soak.sh run` passed after fixing
  octos-tui to render the backend-provided agent summary/last-task detail in
  the visible activity row. Evidence:
  `/Users/yuechen/home/octos/e2e/test-results-m15-task-supervisor-mirror-tmux/m15-task-mirror-tmux-20260516T224202Z`

## Artifact Methods

### `task/artifact/list`

Gate: `harness.task_artifacts.v1`

Params:

```json
{
  "session_id": "local:demo",
  "task_id": "019e0000-0000-7000-8000-000000000001"
}
```

Result:

```json
{
  "artifacts": [
    {
      "id": "review-report",
      "name": "review-report.md",
      "size_bytes": 4812,
      "mime_type": "text/markdown",
      "created_at": "2026-05-15T00:00:31Z"
    }
  ]
}
```

### `task/artifact/read`

Gate: `harness.task_artifacts.v1`

Params:

```json
{
  "session_id": "local:demo",
  "task_id": "019e0000-0000-7000-8000-000000000001",
  "artifact_id": "review-report",
  "cursor": null,
  "limit_bytes": 65536
}
```

Result:

```json
{
  "artifact_id": "review-report",
  "cursor": "next-page-token",
  "text": "# Review\n...",
  "eof": false
}
```

## Notifications

No new notification is required for M13. Lifecycle updates continue to use
`task/updated`, `task/output/delta`, and `turn/spawn_complete`.

## Error Model

Use the existing AppUI JSON-RPC taxonomy and add these typed error kinds:

- `artifact_access_denied`
- `artifact_unavailable`
- `review_target_invalid`

Structured error `data` must include the relevant `session_id`, `task_id`,
`artifact_id`, requested policy field, and the effective denying policy when
available. Secret values must be omitted.

## Security And Runtime Rules

- AppUI clients never construct child tools, memory, sandbox policies, model
  routing, MCP servers, or child prompts.
- Server-owned runtime factories resolve profile, model portfolio, memory,
  tool registry, tool policy, sandbox policy, MCP servers, workspace contract,
  and artifact contract.
- If any runtime dependency is unresolved, backend scheduling fails with a typed
  error. The server must not silently fall back to global tools or default
  models.
- Artifact reads are allowed only for the parent session, the child session, or
  a session that has successfully joined/merged the task under backend policy.
- Internal child tool streams stay inside the child session unless the server
  projects an approved summary/artifact.
- Child-agent context must be fork-scoped and sanitized by the backend. A child
  must not receive the parent turn's raw tool traces, pending calls, reasoning
  fragments, or AppUI transport bookkeeping unless the runtime explicitly
  whitelists them.
- Backend compaction/checkpointing must be applied at turn and sampling
  boundaries before supervised child work is scheduled. AppUI may display a
  compaction/checkpoint item, but must not construct replacement model history.

## Pauli Context-Hygiene Finding

Pauli's Codex comparison found that Codex prevents subagent context pollution
through a few structural mechanisms, not prompt discipline:

- A canonical backend history manager owns model-visible transcript state.
- Tool output is truncated when it is recorded, before later model calls can
  accidentally replay large payloads.
- History is normalized before each model call so orphan tool outputs and
  unsupported content are removed.
- Subagent forks are sanitized: children get selected user/developer context,
  while parent tool outputs, call records, reasoning items, and stale
  `TurnContext`-style baseline state are dropped.
- Compaction replaces backend history with explicit checkpoints at
  turn/sampling boundaries. It is not a TUI-local background timer.

Octos should mirror those rules in the backend runtime factory. AppUI's role is
to expose lifecycle, checkpoint, artifact, and summary items so TUI/web can
inspect what happened after reconnect.

## Compatibility

Older servers reject new methods with `method_not_supported`, or omit new fields
from task payloads. Clients must hide inspection/artifact controls unless the
negotiated capabilities include the matching feature flag.

Existing clients are unaffected because legacy task lifecycle fields remain
unchanged.

## Tests

- AppUI capability negotiation includes the new features only when the server
  supports them.
- `task/list` and `task/updated` expose parent/child task metadata produced by
  backend-owned subagent scheduling.
- Long-running child agents emit heartbeat updates while running.
- The parent turn receives a per-child completion message when each child
  reaches a terminal state.
- The parent turn receives one final scatter-join summary after all children
  complete, fail, or are interrupted.
- Child-agent forks omit raw parent tool outputs/call records and record the
  fork policy in the evidence ledger.
- Compaction/checkpoint events are backend-owned, replayable after reconnect,
  and do not require TUI to rebuild model history.
- TUI can reconnect and hydrate supervised task state without locally inventing
  child state.
- `task/artifact/list` and `task/artifact/read` enforce parent/child/joined
  ownership.
- `review/start` starts product-level code review when advertised.
- Plain `turn/start` review requests still allow backend-owned subagent
  scheduling.
- WebSocket and stdio expose identical method/result/error/event shapes.
- Octos-TUI hides inspection controls on old servers and renders task tree plus
  artifact browser when capabilities are present.
