# Octos UI Protocol Change Request: Task Runtime Cancelled State

## Header

- Request id: `UPCR-2026-004`
- Title: Add `cancelled` variant to `TaskRuntimeState`
- Author: M9 review fixes (coding-green)
- Date: 2026-04-30
- Target protocol: `octos-ui/v1alpha1`
- Status: accepted
- Related M issue: `M9` review (issue #687)

## Summary

This change request adds the `cancelled` variant to the
`TaskRuntimeState` wire enum so the AppUi protocol can faithfully represent
background tasks that were cancelled mid-flight by
`POST /api/tasks/{id}/cancel` and the agent's `TaskSupervisor::cancel`
primitive. The variant is purely additive and does not change any existing
serialization, method, or capability bit.

## Motivation

The agent's `TaskLifecycleState` enum already carries a `Cancelled` terminal
variant (added in M7.9 / W2) and emits it as the snake_case literal
`"cancelled"` whenever a tracked spawn-only task is cancelled before it
reaches `Completed` or `Failed`. The AppUi wire enum
`TaskRuntimeState` had only four variants — `Pending`, `Running`,
`Completed`, `Failed` — so the `ui_task_runtime_state` mapper in
`crates/octos-cli/src/api/ui_protocol_progress.rs` could not match the
`"cancelled"` literal. The `unwrap_or(UiTaskRuntimeState::Running)` fallback
caused cancelled tasks to render as still running indefinitely in the UI.

This UPCR adds the missing variant so the cancel signal is preserved end-to-end
on the wire, eliminating a class of "stuck running" bugs without rewriting any
existing message field.

## Change Type

Additive enum variant.

A new `cancelled` variant on the existing `TaskRuntimeState` enum used by the
`task/updated` notification payload `TaskUpdatedEvent.state`. No method,
notification, required field, capability flag, or protocol identifier
changes.

## Wire Contract

Affected existing wire surface:

- Notification method: `task/updated`
- Notification payload field: `TaskUpdatedEvent.state`
- Wire enum: `TaskRuntimeState`

### Before

`TaskRuntimeState` accepts the snake_case literals:

- `"pending"`
- `"running"`
- `"completed"`
- `"failed"`

### After

`TaskRuntimeState` additionally accepts the snake_case literal:

- `"cancelled"` (canonical wire form, matches British spelling used by the
  agent's `TaskLifecycleState::Cancelled` and `TaskStatus::Cancelled`
  serializers)

### Example wire payload

```json
{
  "jsonrpc": "2.0",
  "method": "task/updated",
  "params": {
    "session_id": "local:demo",
    "task_id": "01900000-0000-7000-8000-000000000003",
    "title": "spawn_only_runner",
    "state": "cancelled",
    "runtime_detail": "user cancelled"
  }
}
```

The mapper additionally accepts the US spelling `"canceled"` defensively, but
the canonical wire form emitted by the server is the British `"cancelled"`.

## Compatibility

- Old clients that exhaustively match on `TaskRuntimeState` and have not been
  recompiled against the new variant will fail to deserialize a `task/updated`
  notification carrying `state: "cancelled"`. This is the standard
  forward-compatibility behaviour for any added enum variant, and is
  acknowledged by the protocol spec (§4 — clients must not assume unknown
  enum variants are impossible forever).
- Clients that deserialize `TaskRuntimeState` permissively or use a closed
  string enum with an `Unknown` fallback continue to work unchanged.
- Old servers that do not emit `"cancelled"` continue to be valid wire
  producers.
- No capability flag is required because the variant is additive on an
  existing field. Servers that have not adopted the variant simply do not
  emit it; the old behaviour (cancelled tasks reported as `running`) was
  already the observable bug, and the field-level upgrade fixes it
  per-server.
- No new protocol identifier is needed.

## Capability Negotiation

None. The variant is additive and does not require a feature flag because
the wire shape (`TaskUpdatedEvent.state` is a single string) is unchanged.

## Tests

- `crates/octos-core/src/ui_protocol.rs`:
  - `task_runtime_state_cancelled_round_trips_as_snake_case_cancelled` — asserts
    the wire literal is exactly `"cancelled"`.
  - `task_updated_event_round_trips_with_cancelled_state` — asserts a full
    `task/updated` notification round-trips through the JSON-RPC envelope.
- `crates/octos-cli/src/api/ui_protocol_progress.rs`:
  - `ui_protocol_progress_maps_cancelled_task_state_to_cancelled_variant`
    — asserts the mapper returns `Cancelled` for both `"cancelled"` and
    `"canceled"` and no longer falls back to `Running`.

## Rollout Plan

This UPCR ships in the same PR that:

1. Adds the variant to `TaskRuntimeState` in `octos-core`.
2. Updates the mapper in `octos-cli/src/api/ui_protocol_progress.rs` to
   recognise `"cancelled"` and `"canceled"`.
3. Adds the round-trip tests above.

No new wire surface is added; no client renegotiation is required. Clients
that consume `task/updated` and want to render the cancelled state explicitly
should adopt the new variant on their next protocol-types update.

## Decision

Accepted by: M9 review fixes (coding-green local).

Decision notes: The variant is the smallest possible change that preserves an
existing terminal state across the wire. The cancelled lifecycle was already
modelled in the agent (`TaskLifecycleState::Cancelled`), the supervisor
(`TaskStatus::Cancelled`), and the UI (`UiTaskRuntimeState::Running` fallback
masked it). Adding the variant restores end-to-end fidelity.
