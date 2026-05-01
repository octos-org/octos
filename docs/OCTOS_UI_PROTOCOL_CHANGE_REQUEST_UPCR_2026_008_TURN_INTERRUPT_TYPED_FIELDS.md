# Octos UI Protocol Change Request: Typed Diagnostic Fields on `TurnInterruptResult`

## Header

- Request id: `UPCR-2026-008`
- Title: Add typed `reason`, `terminal_state`, and `ack_timeout` optional
  fields to `TurnInterruptResult`
- Author: M9 protocol-as-contract audit follow-up (coding-green)
- Date: 2026-04-30
- Target protocol: `octos-ui/v1alpha1`
- Status: accepted
- Related issue: `#721` (server emits ad-hoc JSON fields beyond the typed
  `TurnInterruptResult { interrupted: bool }`)

## Summary

This change request codifies three optional diagnostic fields that the
`turn/interrupt` handler in `crates/octos-cli/src/api/ui_protocol.rs` has been
emitting via raw `serde_json::json!` since the AppUi protocol shipped:

- `reason: String` — non-terminal explanation when `interrupted` is `false`
  (e.g., `turn_id_mismatch`).
- `terminal_state: String` — the prior terminal state when interrupt is
  applied to an already-terminal turn (`completed` / `errored` /
  `interrupted`).
- `ack_timeout: bool` — set to `true` when the server captured the interrupt
  but could not confirm the wire-side terminal event was acknowledged within
  the ack window.

All three fields already cross the wire today; only the typed contract
(`TurnInterruptResult` in `crates/octos-core/src/ui_protocol.rs`) was lagging.
This UPCR brings the type up to the existing wire shape so the contract is
honest. No semantic change.

## Motivation

Audit issue `#721` flagged the drift: the server's `handle_turn_interrupt`
handler emits four distinct response shapes via raw `json!()`, but the typed
result advertised by `octos-core` is the single-field
`TurnInterruptResult { interrupted: bool }`. The drift is exactly the kind of
silent wire/type divergence the protocol-as-contract rule is meant to
prevent. Specifically, the handler emits:

- `{ "interrupted": false, "reason": "turn_id_mismatch" }`
- `{ "interrupted": <bool>, "terminal_state": "completed"|"errored"|"interrupted" }`
- `{ "interrupted": true }`
- `{ "interrupted": true, "ack_timeout": true }`

Of these, only the third matches the typed result. The first, second, and
fourth carry diagnostic information clients have already been free to read
because the v1 contract stipulates that unknown fields must be ignored.
Codifying them in the typed result lets typed clients consume the diagnostic
fields safely without re-parsing the raw `Value`.

## Change Type

Additive optional fields on an existing typed RPC result.

`TurnInterruptResult` gains three fields, each marked
`#[serde(default, skip_serializing_if = "Option::is_none")]` so the canonical
minimal wire shape `{ "interrupted": <bool> }` is unchanged for the typical
cases where the diagnostic fields are absent. No new method, notification,
enum variant, capability flag, feature, or protocol identifier is introduced.

## Wire Contract

Affected wire surface — strictly additive:

- Result type: `TurnInterruptResult` (extended with three optional fields)

No existing field is renamed, removed, or made required. The typed
constructor `TurnInterruptResult::new(interrupted: bool)` is preserved and
fills the new fields with `None`, so all existing callers continue to
produce the canonical `{ "interrupted": <bool> }` wire shape.

### `TurnInterruptResult` (extended)

Result of `turn/interrupt`:

```json
{ "interrupted": true }
```

```json
{ "interrupted": false, "reason": "turn_id_mismatch" }
```

```json
{ "interrupted": false, "terminal_state": "completed" }
```

```json
{ "interrupted": true, "ack_timeout": true }
```

Field descriptions:

- `interrupted` (required, `bool`): canonical interrupt acknowledgement.
  `true` iff the server stopped the turn (or the turn had already been
  interrupted). `false` iff the interrupt was declined or the turn was
  already in a non-`interrupted` terminal state.
- `reason` (optional, `string`): non-terminal diagnostic explanation when
  `interrupted` is `false`. String registry with initial value:
  - `turn_id_mismatch` — the `turn_id` sent does not match the active turn
    for the session.
- `terminal_state` (optional, `string`): set when the interrupt was sent
  against a turn that had already reached a terminal state. String registry,
  values matching the server's `TerminalReason` enum:
  - `completed`
  - `errored`
  - `interrupted`
- `ack_timeout` (optional, `bool`): set to `true` only when the server
  captured the interrupt and emitted the wire-side terminal event but could
  not confirm that the client received the terminal within the server's ack
  window. The interrupt itself is still considered captured (`interrupted`
  is `true`); only client-side receipt is uncertain. Omitted otherwise.

`reason` and `terminal_state` are mutually exclusive in practice (the server
only sets `terminal_state` for already-terminal turns and `reason` for
non-terminal declines), but the wire contract does not forbid both being
present in future revisions; clients should accept either, neither, or both.

Future `reason` and `terminal_state` registry values must be added via a
follow-up UPCR.

## Error Model

This UPCR does not introduce or modify any RPC error category. The existing
`unknown_turn` error continues to be the response when the server has no
record of the requested `turn_id`. The new optional fields are carried only
on the success result.

## Compatibility

- Old clients that ignore unknown fields per spec § 4 see no behavioural
  change. The minimal `{ "interrupted": <bool> }` canonical shape is
  preserved for every case where the diagnostic fields are `None`.
- Old servers that have not yet been recompiled against the new type
  continue to emit the same wire shapes via `json!` (this UPCR does not
  delete the raw-`json!` code path until the typed-builder migration lands).
- New typed clients can deserialize the result directly into the extended
  `TurnInterruptResult` and read `reason`, `terminal_state`, and
  `ack_timeout` for diagnostic UX without re-parsing the raw `Value`.
- The handler in `crates/octos-cli/src/api/ui_protocol.rs` is migrated to
  the typed `TurnInterruptResult` constructors (`interrupted_ok`,
  `declined`, `already_terminal`, `ack_timed_out`) so the wire emission and
  the typed contract are produced from a single source.
- No new protocol identifier or capability flag is required because the
  change is additive and `Option::is_none` is the v1 idiom for additive
  optional fields.

## Capability Negotiation

No new capability flag. v1 forward-compat (unknown fields ignored) is
sufficient for additive optional result fields.

## Tests

- `crates/octos-core/src/ui_protocol.rs`:
  - `turn_interrupt_result_minimal_omits_optional_fields` — golden:
    `interrupted: true` only, with no diagnostic fields, round-trips through
    serde and produces the canonical `{ "interrupted": true }` wire shape.
  - `turn_interrupt_result_round_trips_with_reason` — golden: declined
    interrupt with `reason: Some("turn_id_mismatch")` round-trips.
  - `turn_interrupt_result_round_trips_with_terminal_state` — golden:
    already-terminal interrupt with `terminal_state: Some("completed")`
    round-trips.
  - `turn_interrupt_result_round_trips_with_ack_timeout` — golden:
    ack-timed-out interrupt with `ack_timeout: Some(true)` round-trips.
  - `turn_interrupt_result_decodes_with_unknown_fields_ignored` — forward
    compat: a result with an extra unknown field decodes cleanly.
  - The pre-existing
    `typed_rpc_results_map_from_methods_and_round_trip` test continues to
    pass because the canonical `{ "interrupted": false }` wire shape is
    preserved for `TurnInterruptResult::new(false)`.

## Rollout Plan

1. Land the typed result extension and the four named constructors on
   `TurnInterruptResult` in `octos-core` (this PR).
2. Migrate the four `handle_turn_interrupt` emission sites in `octos-cli`
   from raw `serde_json::json!` to the typed constructors, dispatched
   through a single `send_typed_interrupt_result` helper (this PR).
3. Update the spec § 7 entry for `turn/interrupt` to reference
   `UPCR-2026-008` (this PR).
4. Follow-up: harness clients that currently re-parse the raw `Value` to
   surface `reason` / `terminal_state` / `ack_timeout` may switch to the
   typed result. Tracked separately if/when needed.

No client renegotiation is required.

## Decision

Accepted by: M9 protocol-as-contract audit follow-up (coding-green).

Decision notes: Accepted as the minimum additive contract change to close
audit issue `#721`. The fields have already been on the wire since the
AppUi protocol shipped; this UPCR only brings the typed contract up to the
existing wire shape so the protocol-as-contract rule holds end-to-end.

## Out-of-scope follow-ups

- The `terminal_state` value strings (`completed`, `errored`,
  `interrupted`) match the server's internal `TerminalReason` enum and are
  not yet shared with `octos-core`. A future UPCR may promote
  `TerminalReason` to a typed enum on the wire if more values are added.
- The `reason` registry is intentionally minimal in this UPCR
  (`turn_id_mismatch` only). Future declined-interrupt explanations require
  an extension UPCR.
