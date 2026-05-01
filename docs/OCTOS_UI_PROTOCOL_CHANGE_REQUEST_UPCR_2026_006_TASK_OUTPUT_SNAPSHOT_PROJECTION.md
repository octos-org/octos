# Octos UI Protocol Change Request: Task Output Snapshot-Projection Flag

## Header

- Request id: `UPCR-2026-006`
- Title: Add `is_snapshot_projection: bool` to `task/output/read` result
- Author: M9 harness audit follow-up (coding-green)
- Date: 2026-04-30
- Target protocol: `octos-ui/v1alpha1`
- Status: accepted
- Related M issue: `#707` (M9 req 7 P3 from
  [OCTOS_HARNESS_AUDIT_M6_M9_2026-04-30.md](OCTOS_HARNESS_AUDIT_M6_M9_2026-04-30.md))

## Summary

This change request adds a single additive boolean field
`is_snapshot_projection` to the `TaskOutputReadResult` payload returned by the
`task/output/read` JSON-RPC command. The field tells clients whether the read
was projected from the task ledger snapshot (the only mode the runtime
currently supports) or sourced from a live disk-routed stdout/stderr stream
(the mode targeted by the M8.7 disk-routed output work). The change is purely
additive: no existing field, method, notification, enum variant, capability
flag, or protocol identifier is modified.

## Motivation

The M6-M9 harness audit (`docs/OCTOS_HARNESS_AUDIT_M6_M9_2026-04-30.md` § M9
req 7) flagged that the wire payload for `task/output/read` did not expose
whether the cursor semantics were backed by a real live byte stream or by a
snapshot projection. The audit assigned this gap priority **P3** and tracked
it as issue `#707`.

Today's runtime (`crates/octos-cli/src/api/ui_protocol_task_output.rs:3`)
states explicitly:

> The current runtime persists task snapshots, not disk-routed stdout/stderr
> streams. This module exposes a typed, cursorable projection of that
> snapshot data and reports whether this read source itself can be
> live-tailed.

The result already carries:

- `source: TaskOutputReadSource` — currently the single variant
  `runtime_projection`.
- `live_tail_supported: bool` — currently always `false` for the projection
  source.
- `limitations: Vec<TaskOutputReadLimitation>` — a human-readable list whose
  entries include `live_tail_unavailable` and `disk_output_unavailable`.

Clients can *infer* "this is a snapshot, not a live stream" from these signals
today, but only by enumerating the `runtime_projection` source label or by
parsing the `limitations[]` strings. Neither is a stable contract:

1. `source` is an open `serde(rename_all = "snake_case")` enum that may grow
   new variants (e.g. `disk_routed`, `mcp_streamed`). A client that switches
   on `source == "runtime_projection"` to decide whether the cursor is
   advisory will silently break the moment a second projection-style source
   ships.
2. `limitations[].code` is a free-form string registry; the spec § 7 minimum
   contract for `task/output/read` does not pin a specific limitation code as
   the snapshot indicator.
3. `live_tail_supported` answers a different question — whether the *source*
   has a live-tail mode at all, not whether *this particular response* came
   from a snapshot. A future source might be live-tail-capable in general yet
   serve a single response from a snapshot fallback (e.g. on reconnect).

The audit therefore requires a single, dedicated boolean that says exactly:
"this response was projected from a snapshot; cursors are advisory." That is
the contract `is_snapshot_projection` carries.

The semantic contract attached to the new field:

- `is_snapshot_projection: true` means the `text` window was drawn from a
  point-in-time projection of the task ledger snapshot. The `cursor` and
  `next_cursor` advance through the bytes of that projection, but a fresh
  `task/output/read` request may project a different snapshot (the task
  ledger may have advanced; runtime detail or output_files may have changed).
  Clients must treat the cursor as advisory across reads and must not assume
  that `next_cursor` from an earlier read still points to a valid offset in
  a later snapshot.
- `is_snapshot_projection: false` means the `text` window was drawn from a
  live disk-routed (or otherwise byte-monotonic) source. `next_cursor` is a
  stable offset that a follow-up read can resume from.

## Change Type

Additive field on result payload.

A new required boolean field on the existing `TaskOutputReadResult` struct
returned by the `task/output/read` JSON-RPC command. No method, notification,
enum variant, capability flag, or protocol identifier is modified.

## Wire Contract

Affected existing wire surface — strictly additive:

- Command result: `TaskOutputReadResult`

### Before

```json
{
  "session_id": "local:demo",
  "task_id": "01900000-0000-7000-8000-000000000001",
  "source": "runtime_projection",
  "cursor": { "offset": 0 },
  "next_cursor": { "offset": 6 },
  "text": "output",
  "bytes_read": 6,
  "total_bytes": 6,
  "truncated": false,
  "complete": true,
  "live_tail_supported": false,
  "task_status": "completed",
  "runtime_state": "completed",
  "lifecycle_state": "completed",
  "limitations": [
    { "code": "snapshot_projection", "message": "served from task snapshot" }
  ]
}
```

### After

```json
{
  "session_id": "local:demo",
  "task_id": "01900000-0000-7000-8000-000000000001",
  "source": "runtime_projection",
  "cursor": { "offset": 0 },
  "next_cursor": { "offset": 6 },
  "text": "output",
  "bytes_read": 6,
  "total_bytes": 6,
  "truncated": false,
  "complete": true,
  "live_tail_supported": false,
  "is_snapshot_projection": true,
  "task_status": "completed",
  "runtime_state": "completed",
  "lifecycle_state": "completed",
  "limitations": [
    { "code": "snapshot_projection", "message": "served from task snapshot" }
  ]
}
```

The new field is required and serialized verbatim alongside
`live_tail_supported`. The Rust struct keeps the field as a plain
`pub is_snapshot_projection: bool` (no `serde(default)`), matching the
existing pattern for `live_tail_supported`, `truncated`, `complete`, and
other wire-visible truth booleans on this result.

## Compatibility

- Old clients that deserialize unknown fields permissively (i.e. follow spec
  § 4 "unknown fields MUST be ignored") continue to work. The added field is
  ignored without affecting their handling of the cursor, text, or
  limitations.
- Old clients that exhaustively enumerate fields and reject unknown ones
  must add the field to their deserializer on the next protocol-types
  update. This is the standard forward-compatibility behaviour for any
  added field, acknowledged by spec § 4.
- Old servers that have not adopted the new field cannot satisfy the new
  required-field shape. Servers in this repository all flow through the
  shared `project_task_output` constructor and therefore set the field in
  one place; downstream forks must add the field at construction time.
- No capability flag is required because the field is additive on an
  existing result and the wire shape (`TaskOutputReadResult` is a single
  JSON object) is unchanged.
- The field is a strict refinement of information already implied by
  `source == "runtime_projection"` plus `limitations[].code` membership of
  `live_tail_unavailable`/`snapshot_projection`. Servers that emit those
  signals correctly today will set `is_snapshot_projection: true` for the
  same reads.

## Capability Negotiation

None. The field is additive on an existing result and does not require a
feature flag.

## Tests

- `crates/octos-core/src/ui_protocol.rs`:
  - `representative_payload_round_trip_v1` — extended with
    `is_snapshot_projection: true` on the literal `task/output/read` golden
    payload.
  - `typed_rpc_results_map_from_methods_and_round_trip` — extended with an
    explicit assertion that the wire object carries
    `"is_snapshot_projection": true` for `runtime_projection` results.
- `crates/octos-cli/src/api/ui_protocol_task_output.rs`:
  - `runtime_projection_serializes_stable_live_tail_metadata` — extended
    with a wire-shape assertion that `value["is_snapshot_projection"] ==
    true`. The handler's `project_task_output` always sets the field to
    `true` until a non-snapshot source ships.
- `e2e/tests/m9-protocol-task-output-read.spec.ts`:
  - `initial read + follow-up read advance by task/output cursor` — extended
    with an explicit assertion that `first.is_snapshot_projection === true`
    and `first.live_tail_supported === false`.

## Rollout Plan

1. Land the field on `TaskOutputReadResult`, the in-handler default
   (`is_snapshot_projection: true`), the golden-test updates, and the e2e
   client type/test updates in this PR.
2. Update this spec § 7 `task/output/read` block to reference UPCR-2026-006
   and document the field's contract.
3. Follow-up: once the M8.7 disk-routed task stdout/stderr work ships, the
   disk-routed constructor will set `is_snapshot_projection: false` and the
   matching `live_tail_supported: true`. That follow-up does not require a
   new UPCR — the field is already accepted on the wire by this UPCR.

No client renegotiation is required for clients that follow the spec's
"unknown fields MUST be ignored" rule. Clients that want to render the
"this is a stale snapshot" hint in their UI should adopt the field on
their next protocol-types update.

## Decision

Accepted by: M9 harness audit follow-up (coding-green).

Decision notes: Accepted as the minimum additive contract change to close
audit issue #707 (M9 req 7 P3). The new field gives clients a single,
stable wire-level signal for snapshot vs. live-tail semantics that does
not depend on enumerating an open `source` enum or parsing free-form
`limitations[]` strings. The field is required so servers cannot
accidentally omit it and so clients can rely on it without an
"unspecified means false" fallback.
