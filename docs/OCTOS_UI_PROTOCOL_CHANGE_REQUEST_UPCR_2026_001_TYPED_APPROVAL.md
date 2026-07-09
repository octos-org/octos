# Octos UI Protocol Change Request: Typed Approval Payloads

## Header

- Request id: `UPCR-2026-001`
- Title: Add typed approval request/response fields
- Author: M9 protocol workstream
- Date: 2026-04-24
- Target protocol: `octos-ui/v1alpha1`
- Status: accepted
- Related issue: M9 protocol contract

## Summary

This change request codifies the approval fields that make
`approval/requested`, `approval/respond`, and `approval/scopes/list`
machine-readable without removing the original generic approval flow.

The change is additive. Existing clients can still answer a pending approval
with only `session_id`, `approval_id`, and `decision`. New clients can render
typed request details, submit an advisory response scope, include a client
note, and query the current scoped approval decisions.

## Wire Contract

Feature flag:

- `approval.typed.v1`

Methods:

- `approval/respond`
- `approval/scopes/list`

Notifications:

- `approval/requested`
- `approval/auto_resolved`
- `approval/decided`
- `approval/cancelled`

### `approval/requested`

The existing notification keeps its legacy fields and may include typed
details for command/tool approvals:

- `typed_details`: structured command/tool context for rendering
- `risk`: optional manifest or tool risk classification
- `render_hints`: optional UI guidance

Clients must tolerate missing typed fields because old servers and generic
approval producers can still emit the legacy shape.

### `approval/respond`

Minimum params remain:

- `session_id`
- `approval_id`
- `decision`

Optional params:

- `approval_scope`: advisory string registry. Initial accepted values are
  `request`, `tool`, `turn`, and `session`; compatibility aliases such as
  `approve_for_tool`, `approve_for_turn`, and `approve_for_session` normalize
  to the same registry values.
- `client_note`: human-readable audit/display note.

The response scope must not silently create persistent allow rules beyond the
server-owned approval policy. Unknown scopes are accepted as open-registry
strings only when the server can preserve the literal in audit state; otherwise
the server should reject with `invalid_params`.

### `approval/scopes/list`

Purpose:

- return the server-recorded scoped approval decisions for a session

Minimum params:

- `session_id`

Result:

```json
{
  "scopes": [
    {
      "approval_id": "appr_...",
      "scope": "session",
      "decision": "approve",
      "client_note": "looks good"
    }
  ]
}
```

## Compatibility

- `approval/respond` stays backwards-compatible: absent `approval_scope` and
  `client_note` decode as `null`.
- Old clients that ignore typed approval fields continue to receive the
  legacy `approval/requested` information.
- The typed approval feature does not change JSON-RPC error codes.
- `approval/auto_resolved`, `approval/decided`, and `approval/cancelled` are
  durable notification records used for replay and audit. Clients should treat
  unknown fields as ignorable.

## Tests

Coverage lives in:

- `crates/octos-core/src/ui_protocol.rs`
  - approval response params round-trip optional `approval_scope` and
    `client_note`
  - `approval/scopes/list` result round-trips through `UiRpcResult`
  - typed `approval/requested` details round-trip
- `crates/octos-cli/src/api/ui_protocol.rs`
  - scoped approval enforcement and scope listing
  - durable `approval/decided` / `approval/cancelled` replay behavior
- `e2e/tests/m9-protocol-approval-respond.spec.ts`
  - WebSocket acceptance of optional `approval_scope` and `client_note`

## Decision

Accepted by the M9 protocol workstream. The approval shape was already shipped
in code; this document restores the missing change-control record so the spec,
source, and route inventory agree.
