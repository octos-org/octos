# Octos UI Protocol Change Request: Task Control RPCs

## Header

- Request id: `UPCR-2026-005`
- Title: Add `task/list`, `task/cancel`, `task/restart_from_node` RPC commands
- Author: M9 harness audit follow-up (coding-green)
- Date: 2026-04-30
- Target protocol: `octos-ui/v1alpha1`
- Status: accepted
- Related M issue: `#704` (M9 req 9 P2 from
  [OCTOS_HARNESS_AUDIT_M6_M9_2026-04-30.md](OCTOS_HARNESS_AUDIT_M6_M9_2026-04-30.md))

## Summary

This change request adds three additive JSON-RPC command methods to the AppUi
protocol so harness clients can control the runtime task registry without
falling back to REST: `task/list` (enumerate tasks for a session), `task/cancel`
(cancel a running task by id), and `task/restart_from_node` (operator-triggered
relaunch from a specific pipeline node). The methods wrap the stable
`TaskSupervisor` primitives already used by the REST surface and the agent's
internal cancel/relaunch lifecycle. The change is purely additive: no existing
method, payload, enum variant, capability bit, or protocol identifier is
modified.

## Motivation

The M6-M9 harness audit (`docs/OCTOS_HARNESS_AUDIT_M6_M9_2026-04-30.md` § M9
req 9) flagged that `task/cancel` was missing from the `UiCommand` enum. Slash
commands such as `/stop` against a background task and `/ps` against the task
registry had no canonical AppUi RPC entry point and were forced to either
short-circuit to REST or skip the UX entirely. The audit assigned this gap
priority **P2** and tracked it as issue `#704`.

`TaskSupervisor` already exposes `cancel(task_id)` and `relaunch(task_id, opts)`
as stable in-process primitives, and the supervisor enforces the cancel-race
guard added in PR #709 — once a task transitions to `Cancelled`, later
state-transition attempts are no-ops, so a re-entrant cancel cannot overwrite a
terminal state. The agent runtime also exposes a
`SessionTaskQueryStore` that returns a JSON snapshot of all tasks scoped to a
session. Lifting these three primitives to first-class AppUi RPCs preserves the
existing semantics and removes the REST detour.

`task/list` and `task/restart_from_node` are bundled into the same UPCR because
they consume the same supervisor surface, share the same session-scoping rules,
and together form the minimum command set the `/ps` slash command needs. Adding
them piecemeal would require three UPCRs for one logical contract addition.

## Change Type

Additive method.

Three new JSON-RPC command methods on the existing AppUi v1alpha1 protocol.
No new notifications are added. No existing method, notification, required
field, enum variant, or capability flag is modified. One additive feature
flag (`harness.task_control.v1`) is added so clients can negotiate
availability.

## Wire Contract

Affected wire surface — strictly additive:

- Capability payload: `UiProtocolCapabilities` (new feature flag entry,
  full-protocol method-set entry)
- Capability feature registry: `harness.task_control.v1`
- Command method: `task/list` (new)
- Command method: `task/cancel` (new)
- Command method: `task/restart_from_node` (new)
- Command params: `TaskListParams`, `TaskCancelParams`,
  `TaskRestartFromNodeParams` (new)
- Command results: `TaskListResult`, `TaskCancelResult`,
  `TaskRestartFromNodeResult` (new)

No existing command method, notification, params, results, or enum variants
are modified by this UPCR.

### `task/list`

Purpose:

- Enumerate tasks the runtime tracks for a session, with one entry per task
  including lifecycle state, runtime state, optional child-session linkage, and
  output cursors. Primary consumer: the `/ps`-style task panel.

Params:

```json
{
  "session_id": "local:demo",
  "topic": "default"
}
```

- `session_id` (required): canonical session identifier.
- `topic` (optional): sub-topic suffix appended as `<session>#<topic>` for
  grouping; the server falls back to the bare session if omitted or empty.

Result:

```json
{
  "session_id": "local:demo",
  "topic": "default",
  "tasks": [
    {
      "id": "01900000-0000-7000-8000-000000000001",
      "tool_name": "spawn_only_runner",
      "tool_call_id": "call-1",
      "state": "running",
      "status": "running",
      "lifecycle_state": "running",
      "runtime_state": "executing_tool",
      "parent_session_key": "local:demo",
      "child_session_key": "local:demo#child-1",
      "started_at": "2026-04-30T12:00:00Z",
      "updated_at": "2026-04-30T12:01:00Z",
      "output_files": ["octos-file://task-output"]
    }
  ]
}
```

- `tasks[].state` is the canonical wire enum `TaskRuntimeState` (the same enum
  used by `task/updated`), derived from the supervisor's `lifecycle_state` /
  `runtime_state` / `status` snapshot. Optional fields follow the existing
  `task/updated` shape and are omitted when empty.

### `task/cancel`

Purpose:

- Cancel a single task in the supervisor and return its final wire state. Maps
  directly to `TaskSupervisor::cancel(task_id)`.

Params:

```json
{
  "task_id": "01900000-0000-7000-8000-000000000001",
  "session_id": "local:demo",
  "profile_id": "coding"
}
```

- `task_id` (required): the task to cancel.
- `session_id` (wire-optional, validated as required at handler time): scopes
  the cancel to one session and enables the same `validate_session_scope`
  check used by other AppUi commands. The wire schema keeps the field
  optional to match the existing v1 pattern of using `serde(default,
  skip_serializing_if = "Option::is_none")` for cross-session-shaped
  identifiers; the handler rejects an absent `session_id` with
  `invalid_params` so clients cannot cross-cancel tasks across sessions.
- `profile_id` (optional): forwarded to the connection-profile validator.

Result:

```json
{
  "task_id": "01900000-0000-7000-8000-000000000001",
  "status": "cancelled"
}
```

- `status` is the canonical `TaskRuntimeState` value `cancelled` (governed by
  accepted `UPCR-2026-004`). The server preserves the cancel-race guard from
  PR #709: once a task is `Cancelled`, later runtime state transitions cannot
  overwrite the supervisor's stored state. A re-cancel of an already-terminal
  task surfaces as `invalid_params` with `data.kind = "task_already_terminal"`
  rather than a second `cancelled` success — the supervisor *state* is the
  idempotent invariant, not the wire response.

### `task/restart_from_node`

Purpose:

- Operator-triggered relaunch of a previously failed or terminal task,
  optionally beginning from a specific pipeline node. Maps to
  `TaskSupervisor::relaunch(task_id, RelaunchOpts { from_node })`.

Params:

```json
{
  "task_id": "01900000-0000-7000-8000-000000000001",
  "node_id": "design",
  "session_id": "local:demo",
  "profile_id": "coding"
}
```

- `task_id` (required): the task to relaunch.
- `node_id` (optional): pipeline node id to resume from. Forwarded to
  `RelaunchOpts.from_node`.
- `session_id` (wire-optional, validated as required at handler time): same
  scoping rule as `task/cancel`.
- `profile_id` (optional): forwarded to the connection-profile validator.

Result:

```json
{
  "original_task_id": "01900000-0000-7000-8000-000000000001",
  "new_task_id": "01900000-0000-7000-8000-000000000002",
  "from_node": "design"
}
```

- `new_task_id` is the supervisor-assigned id of the relaunched successor.
- `from_node` echoes the requested node when the supervisor accepted it.

## Error Model

The new commands return errors from the existing v1 taxonomy
([§ 10](../api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md)):

- `unknown_task` — supervisor has no task with that `task_id`, or the task is
  scoped to a different session than the request. Returned as `RpcError`
  category `unknown_task`.
- `invalid_params` — params failed structural validation. Includes:
  - missing `session_id` on `task/cancel` / `task/restart_from_node`,
  - `task_already_terminal` (cancel applied to a task already in a terminal
    state, including a task that was already cancelled) — returned with
    `data.kind = "task_already_terminal"`,
  - `task_still_active` (relaunch applied to a non-terminal task) — returned
    with `data.kind = "task_still_active"`,
  - profile-scope mismatches surfaced through the existing
    `validate_session_scope` helper. Carries the same
    `expected_profile_id` / `actual_profile_id` data fields the rest of the
    AppUi command surface already returns; this UPCR keeps the existing
    convention rather than introducing a new `permission_denied` channel for
    the same case.
- `runtime_unavailable` — server has no `task_query_store` wired for this
  deployment (e.g. lite/embedded build). Returned with
  `data.kind = "runtime_unavailable"`.
- `internal_error` — supervisor produced a non-array snapshot or returned an
  unparseable task id (defensive; should not occur in practice).

A `task/list` request for an inactive or unknown session returns an empty
`tasks` array rather than `unknown_session`, matching how the existing
`SessionTaskQueryStore` snapshot already handles missing supervisors.

No new error categories are introduced.

## Compatibility

- Old clients that never request `harness.task_control.v1` and never send
  `task/list` / `task/cancel` / `task/restart_from_node` are unaffected.
- Old servers that have not implemented these methods reject incoming
  `task/*` requests with the existing `method_not_supported` error
  (`UI_PROTOCOL_FIRST_SERVER_METHODS` membership check).
- No new protocol identifier is required because all three methods are
  additive.
- Clients that exhaustively match on `UiCommand` or `UiResultKind` and have not
  been recompiled against the new enum variants will fail to deserialize a
  message carrying one of the new methods. This is the standard
  forward-compatibility behaviour for any added method, acknowledged by
  spec § 4.1.
- The cancel-race guard from PR #709 (terminal `Cancelled` cannot be
  overwritten) is preserved end-to-end: `task/cancel` simply lifts the
  supervisor's existing cancel call onto the wire.

## Capability Negotiation

New feature flag:

- `harness.task_control.v1`

Servers advertise it through `UiProtocolCapabilities.supported_features`. The
flag is included in `UiProtocolCapabilities::full_protocol()` and in the first
server slice's supported method set. Clients that want to depend on the new
methods should request the feature through the existing
`X-Octos-Ui-Features` header or the `ui_feature` / `ui_features` query
parameters.

If a client sends a `task/*` method to a server that does not advertise the
feature, the server returns the existing `method_not_supported` error.

## Tests

- `crates/octos-core/src/ui_protocol.rs`:
  - `task_control_commands_build_and_parse_json_rpc_requests` — round-trips
    `task/list`, `task/cancel`, `task/restart_from_node` requests through the
    JSON-RPC envelope.
  - `typed_rpc_results_map_from_methods_and_round_trip` — extended with
    `TaskListResult`, `TaskCancelResult`, `TaskRestartFromNodeResult` golden
    coverage.
  - `full_protocol_capabilities_advertise_harness_task_control` — asserts the
    new feature flag and methods are advertised in `full_protocol()`.
  - Capability-set golden tests updated to include the new methods and the
    `harness.task_control.v1` feature literal.
- `crates/octos-core/src/app_ui.rs`:
  - `app_command_surface_covers_harness_task_control` — exercises the
    `AppUiCommand` adapter for all three methods.
- `crates/octos-cli/src/api/ui_protocol.rs`:
  - `appui_task_list_returns_runtime_snapshot` — verifies the handler returns
    the supervisor snapshot for a session.
  - `appui_task_cancel_uses_supervisor_cancel_path` — verifies the handler
    routes to `TaskSupervisor::cancel` and reports `cancelled`.
  - `appui_task_restart_from_node_uses_relaunch_path` — verifies the handler
    routes to `TaskSupervisor::relaunch` and emits a fresh `new_task_id`.
  - Routing tests extended with `task/list`, `task/cancel`,
    `task/restart_from_node` requests.

## Rollout Plan

1. Land the protocol constants, params/result types, command/result enums,
   capability flag, and golden tests in `octos-core`.
2. Land the server handlers (`handle_task_list`, `handle_task_cancel`,
   `handle_task_restart_from_node`), the `SessionTaskQueryStore` projection,
   and the routing tests in `octos-cli`.
3. Update this spec to reference UPCR-2026-005 from § 7.
4. Follow-up: TUI `/ps` slash command surface (separate change, tracked under
   #704 follow-ups) consumes the new methods.

No client renegotiation is required for clients that do not consume the new
methods.

## Decision

Accepted by: M9 harness audit follow-up (coding-green).

Decision notes: Accepted as the minimum additive contract change to close
audit issue #704 (M9 req 9 P2). The three methods wrap stable supervisor
primitives, preserve the PR #709 cancel-race guard, and reuse the existing
v1 error taxonomy. TUI-side `/ps` UX remains a follow-up; this UPCR scopes
only the wire contract.
