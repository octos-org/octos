# UPCR-2026-022: Wire Contracts Backfill For 8 Emitted Notifications

Status: Accepted
Date: 2026-05-27

## Summary

Document the wire contract for eight AppUI notification kinds that reach
production WebSocket / stdio clients but were never described in
`OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md` § 8 ("Event Semantics"). The events
ship in `octos-cli` today; this UPCR backfills their typed payload shape so
clients can rely on a stable field set without reading server code.

The eight kinds are:

1. `turn/spawn_complete` (M10 Phase 1 — `event.spawn_complete.v1`)
2. `context/compaction_completed` (M16 — `context.lifecycle.v1`)
3. `context/normalization_reported` (M16 — `context.lifecycle.v1`)
4. `approval/auto_resolved` (UPCR-2026-007 capability list)
5. `approval/decided` (UPCR-2026-007 capability list)
6. `approval/cancelled` (UPCR-2026-007 capability list)
7. `progress/updated` (UPCR-2026-007 capability list)
8. `protocol/replay_lossy` (referenced by spec § 9 backpressure language)

## Motivation

Each emitter shipped ahead of the spec amendment. UPCR-2026-007 advertises
`approval/auto_resolved`, `approval/decided`, `approval/cancelled`,
`progress/updated`, and `protocol/replay_lossy` in `supported_notifications`,
but the spec body only describes `approval/requested`. UPCR-2026-019 mentions
`turn/spawn_complete` as part of the M10 spawn-only completion path without
specifying its payload. The M16 context-manager work introduced
`context/compaction_completed` and `context/normalization_reported` without a
matching § 8 entry.

Clients that try to decode these events strongly today must derive the field
set from `crates/octos-core/src/ui_protocol.rs`. This UPCR pins each contract
to the production struct definition so a future change requires a new UPCR
rather than a silent server-side rename.

No emitter changes. No new capability gates. No new field on the wire. This is
docs-only — the field set documented below is exactly what
`crates/octos-core/src/ui_protocol.rs` already serializes today.

## Relationship To Existing UPCRs

- UPCR-2026-007 first listed the four approval/progress/replay kinds in the
  capability advertisement payload.
- UPCR-2026-014 (M9-α-9) introduced `protocol/replay_lossy` as the
  backpressure-diverge signal that companions the durable ledger.
- UPCR-2026-019 referenced `turn/spawn_complete` as part of the M10 spawn-only
  completion contract.
- The M16 context-manager workstream (`OCTOS_CONTEXT_MANAGER_GAP_CONTRACT`)
  introduced the two `context/*` lifecycle notifications.

This UPCR does not modify any of those decisions. It backfills the wire
contract documentation that those prior UPCRs deferred.

## Capability Gates

| Kind                              | Capability gate                  | Notes                                                                            |
|-----------------------------------|----------------------------------|----------------------------------------------------------------------------------|
| `turn/spawn_complete`             | `event.spawn_complete.v1`        | When not negotiated, the same row appears as `message/persisted` instead.        |
| `context/compaction_completed`    | `context.lifecycle.v1`           | Omitted entirely when the capability is not advertised.                          |
| `context/normalization_reported`  | `context.lifecycle.v1`           | Omitted entirely when the capability is not advertised.                          |
| `approval/auto_resolved`          | Implicit in v1alpha1 baseline    | Listed in UPCR-2026-007 `supported_notifications`.                               |
| `approval/decided`                | Implicit in v1alpha1 baseline    | Listed in UPCR-2026-007 `supported_notifications`.                               |
| `approval/cancelled`              | Implicit in v1alpha1 baseline    | Listed in UPCR-2026-007 `supported_notifications`.                               |
| `progress/updated`                | Implicit in v1alpha1 baseline    | Listed in UPCR-2026-007 `supported_notifications`.                               |
| `protocol/replay_lossy`           | Implicit in v1alpha1 baseline    | Listed in UPCR-2026-007 `supported_notifications`; spec § 9 references it.       |

## Wire Contracts

### `turn/spawn_complete`

Source struct: `TurnSpawnCompleteEvent`
(`crates/octos-core/src/ui_protocol.rs`).

Completion-as-new-envelope event for `spawn_only` background tool results.
Carries the late assistant `content` + `media` plus the originating user
prompt's `client_message_id` (`response_to_client_message_id`) so the client
can render the result as a NEW assistant bubble under the correct user prompt
— without splice-merging into the existing spawn-acknowledgement bubble.

When `event.spawn_complete.v1` is **not** negotiated, the durable row is
still committed to the session ledger and surfaces as `message/persisted` for
backwards compatibility.

| Field                            | Type                     | Required | Semantics                                                                                                                            |
|----------------------------------|--------------------------|:--------:|--------------------------------------------------------------------------------------------------------------------------------------|
| `session_id`                     | `SessionKey`             | yes      | Owning session.                                                                                                                      |
| `topic`                          | string                   | no       | Sub-topic suffix (`<session>#<topic>`) when the turn is topic-scoped.                                                                |
| `turn_id`                        | `TurnId`                 | no       | Originating turn id. Optional for legacy emitters that did not propagate it.                                                         |
| `thread_id`                      | string                   | no       | Originating thread id when threads are in use.                                                                                       |
| `task_id`                        | string                   | yes      | The `spawn_only` task that produced this completion. A `turn/spawn_complete` without `task_id` is a server bug.                      |
| `tool_call_id`                   | string                   | no       | Originating tool call id so the client can flip the in-flight chip from spinner to checkmark without `task_id → tool_call_id` maps.  |
| `response_to_client_message_id`  | string                   | no       | Anchor user prompt's `client_message_id`. Absent only for legacy callers that did not propagate origination through the spawn path.  |
| `seq`                            | u64                      | yes      | Durable ledger sequence number.                                                                                                      |
| `message_id`                     | string                   | yes      | Stable per-row id (`session:seq:timestamp_ns`).                                                                                      |
| `source`                         | string                   | yes      | Origin of the completion. Always `background` today; reserved as string so future variants (e.g. `recovery_background`) can extend.  |
| `cursor`                         | `UiCursor`               | yes      | Cursor immediately after this event for resume on reconnect.                                                                         |
| `persisted_at`                   | RFC 3339 timestamp       | yes      | Time of durable commit.                                                                                                              |
| `content`                        | string                   | yes      | Full assistant text for the completion bubble. Carried inline so the client renders the new bubble atomically without a follow-up.   |
| `media`                          | array<string>            | no       | File attachments produced by the spawn (e.g. `_report.md`, `output.mp3`). Same convention as `MessagePersistedEvent.media`.          |

Replay behavior: lossless (durable ledger event).

Backward compatibility: clients without `event.spawn_complete.v1` continue to
receive `message/persisted` for the same row, with the spawn completion's
content/media inline.

### `context/compaction_completed`

Source struct: `ContextCompactionCompletedEvent`
(`crates/octos-core/src/ui_protocol.rs`).

Notification that a server-owned context-manager compaction pass committed.
Carries the post-compaction context state plus a typed compaction record.
Emitted by `appui_context_compaction_notification` in
`crates/octos-cli/src/api/ui_protocol.rs`.

| Field            | Type                          | Required | Semantics                                                              |
|------------------|-------------------------------|:--------:|------------------------------------------------------------------------|
| `session_id`     | `SessionKey`                  | yes      | Owning session.                                                        |
| `context_state`  | `UiContextState`              | yes      | Server-owned context summary after compaction.                         |
| `compaction`     | `UiContextCompactionRecord`   | yes      | Typed compaction record (counts, hashes, status, error).               |

`UiContextState` fields:

| Field                  | Type   | Required | Semantics                                                          |
|------------------------|--------|:--------:|--------------------------------------------------------------------|
| `session_id`           | string | yes      | Owning session.                                                    |
| `thread_id`            | string | no       | Owning thread when threads are in use.                             |
| `generation`           | u64    | yes      | Monotonic generation counter for the active prompt context.        |
| `transcript_hash`      | string | yes      | Hash of the canonical transcript at this generation.               |
| `item_count`           | usize  | yes      | Count of items retained in the active context.                     |
| `token_estimate`       | usize  | yes      | Estimated token count for the active context.                      |
| `recovery_state`       | string | yes      | Recovery state token (server-defined enum surfaced as string).     |
| `last_checkpoint_id`   | string | no       | Most recent checkpoint id.                                         |
| `last_compaction_id`   | string | no       | Most recent compaction id.                                         |

`UiContextCompactionRecord` fields:

| Field                            | Type   | Required | Semantics                                                            |
|----------------------------------|--------|:--------:|----------------------------------------------------------------------|
| `compaction_id`                  | string | yes      | Stable id for this compaction pass.                                  |
| `checkpoint_id`                  | string | yes      | Checkpoint that produced this compaction.                            |
| `status`                         | string | yes      | Pass status (server-defined enum surfaced as string).                |
| `policy_id`                      | string | yes      | Policy that chose retention / drop rules.                            |
| `trigger`                        | string | yes      | What triggered the pass (server-defined enum surfaced as string).    |
| `input_generation`               | u64    | yes      | Source generation the pass ran against.                              |
| `output_generation`              | u64    | no       | Generation of the replacement transcript when successful.            |
| `input_transcript_hash`          | string | yes      | Hash of the transcript fed into compaction.                          |
| `replacement_transcript_hash`    | string | no       | Hash of the proposed replacement transcript.                         |
| `installed_transcript_hash`      | string | no       | Hash of the installed transcript when the replacement was accepted.  |
| `input_item_count`               | usize  | yes      | Item count before compaction.                                        |
| `retained_count`                 | usize  | yes      | Items retained verbatim.                                             |
| `dropped_count`                  | usize  | yes      | Items dropped.                                                       |
| `summary_item_id`                | string | no       | Synthetic summary item id when one was synthesized.                  |
| `token_estimate_before`          | usize  | yes      | Pre-compaction token estimate.                                       |
| `token_estimate_after`           | usize  | no       | Post-compaction token estimate when the pass succeeded.              |
| `error`                          | string | no       | Error message when the pass failed.                                  |

Replay behavior: lossless (durable ledger event).

Backward compatibility: clients without `context.lifecycle.v1` do not see
this notification.

### `context/normalization_reported`

Source struct: `ContextNormalizationReportedEvent`
(`crates/octos-core/src/ui_protocol.rs`).

Notification that a prompt-normalization pass ran ahead of an LLM call.
Carries counts of repaired / dropped / synthetic / truncated items so AppUI
can render context-hygiene status without re-running normalization locally.
Emitted by `appui_context_normalization_notification` in
`crates/octos-cli/src/api/ui_protocol.rs`.

| Field             | Type                                | Required | Semantics                                                |
|-------------------|-------------------------------------|:--------:|----------------------------------------------------------|
| `session_id`      | `SessionKey`                        | yes      | Owning session.                                          |
| `context_state`   | `UiContextState`                    | yes      | Active context after normalization. See shape above.     |
| `normalization`   | `UiContextNormalizationReport`      | yes      | Counts for this normalization pass.                      |

`UiContextNormalizationReport` fields:

| Field                  | Type   | Required | Semantics                                                                   |
|------------------------|--------|:--------:|-----------------------------------------------------------------------------|
| `generation`           | u64    | yes      | Context generation this report describes.                                   |
| `input_transcript_hash`| string | yes      | Hash of the input transcript.                                               |
| `output_prompt_hash`   | string | yes      | Hash of the normalized output prompt.                                       |
| `model_capability_id`  | string | yes      | Effective model capability id used to choose normalization rules.           |
| `prompt_message_count` | usize  | yes      | Message count in the produced prompt.                                       |
| `token_estimate`       | usize  | yes      | Token estimate for the produced prompt.                                     |
| `repaired_count`       | usize  | yes      | Items that required repair (e.g. orphan tool outputs reattached).           |
| `dropped_count`        | usize  | yes      | Items dropped during normalization.                                         |
| `synthetic_count`      | usize  | yes      | Synthetic items inserted (e.g. placeholders / summary stubs).               |
| `truncated_count`      | usize  | yes      | Items whose payload was truncated.                                          |

Replay behavior: lossless (durable ledger event).

Backward compatibility: clients without `context.lifecycle.v1` do not see
this notification.

### `approval/auto_resolved`

Source struct: `ApprovalAutoResolvedEvent`
(`crates/octos-core/src/ui_protocol.rs`).

Notification emitted when an incoming approval request was auto-resolved by
a previously recorded scope policy entry instead of surfacing a fresh
`approval/requested` to the client.

| Field          | Type                  | Required | Semantics                                                                       |
|----------------|-----------------------|:--------:|---------------------------------------------------------------------------------|
| `session_id`   | `SessionKey`          | yes      | Owning session.                                                                 |
| `approval_id`  | `ApprovalId`          | yes      | Auto-resolved approval id.                                                      |
| `turn_id`      | `TurnId`              | yes      | Originating turn id.                                                            |
| `tool_name`    | string                | yes      | Tool that requested the approval.                                               |
| `scope`        | string                | yes      | Scope key that matched the auto-resolution policy entry.                        |
| `scope_match`  | string                | yes      | Specific match value within `scope` (e.g. command-prefix or path-pattern hit).  |
| `decision`     | `ApprovalDecision`    | yes      | Wire-encoded `approve` / `deny` / forward-compat `Unknown(string)` value.       |

Replay behavior: lossless (durable ledger event).

Backward compatibility: the kind has shipped since UPCR-2026-007's
`supported_notifications` list. Older clients that ignored it continue to
work; clients that decoded it now have a documented field set.

### `approval/decided`

Source struct: `ApprovalDecidedEvent`
(`crates/octos-core/src/ui_protocol.rs`).

Durable record of an approval decision (manual or auto-resolved). Replayed on
reconnect so a client that connected after the decision renders the approval
card as Decided rather than as still pending. Carries identifiers and
decision metadata only; payload bodies (command strings, diffs) are
intentionally omitted for compliance / PII reasons.

| Field            | Type                  | Required | Semantics                                                                                |
|------------------|-----------------------|:--------:|------------------------------------------------------------------------------------------|
| `session_id`     | `SessionKey`          | yes      | Owning session.                                                                          |
| `approval_id`    | `ApprovalId`          | yes      | Approval being decided.                                                                  |
| `turn_id`        | `TurnId`              | yes      | Originating turn id.                                                                     |
| `decision`       | `ApprovalDecision`    | yes      | Wire-encoded `approve` / `deny` / forward-compat `Unknown(string)` value.                |
| `scope`          | string                | no       | Scope under which the decision was recorded (when an auto-resolution policy was created).|
| `decided_at`     | RFC 3339 timestamp    | yes      | Time of the decision.                                                                    |
| `decided_by`     | string                | yes      | Actor that produced the decision (`user:<id>`, `policy:<id>`, etc.).                     |
| `auto_resolved`  | bool                  | yes      | Whether a previously-recorded scope policy auto-resolved the request. Defaults to false. |
| `policy_id`      | string                | no       | Policy id used for auto-resolution.                                                      |
| `client_note`    | string                | no       | Optional free-form note attached by the deciding client.                                 |

Replay behavior: lossless (durable ledger event).

Backward compatibility: emitted in production since UPCR-2026-007's capability
advertisement landed.

### `approval/cancelled`

Source struct: `ApprovalCancelledEvent`
(`crates/octos-core/src/ui_protocol.rs`).

Durable notification announcing that a previously pending approval was
cancelled by the server before any client could respond. Today the only
emitted reason is `turn_interrupted`, but the registry is intentionally open
so future drains (e.g. `session_closed`) can extend without a new event kind.

| Field          | Type           | Required | Semantics                                                                                                                                                                              |
|----------------|----------------|:--------:|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `session_id`   | `SessionKey`   | yes      | Owning session.                                                                                                                                                                        |
| `approval_id`  | `ApprovalId`   | yes      | Approval being cancelled.                                                                                                                                                              |
| `turn_id`      | `TurnId`       | yes      | Originating turn id.                                                                                                                                                                   |
| `reason`       | string         | yes      | Reason code from the open `approval_cancelled_reasons` registry. Initial values: `turn_interrupted`. Unknown reasons must be treated as opaque strings and rendered with generic copy. |

Replay behavior: lossless (durable ledger event).

Backward compatibility: emitted in production since UPCR-2026-007's capability
advertisement landed. The reason registry is open and additive — new entries
do not require a new event kind.

### `progress/updated`

Source struct: `UiProgressEvent` (aliased as `ProgressUpdatedEvent` —
`crates/octos-core/src/ui_protocol.rs`).

Standalone rich progress notification payload. Used for kinds that do not fit
the first-wave `turn/*`, `tool/*`, or `task/*` envelopes — status pills,
retry-with-backoff banners, file-mutation notices, and token / cost
heartbeats.

| Field          | Type                  | Required | Semantics                                                                              |
|----------------|-----------------------|:--------:|----------------------------------------------------------------------------------------|
| `session_id`   | `SessionKey`          | yes      | Owning session.                                                                        |
| `turn_id`      | `TurnId`              | no       | Owning turn when the progress is turn-scoped.                                          |
| `metadata`     | `UiProgressMetadata`  | yes      | Rich metadata block (see below).                                                       |

`UiProgressMetadata` fields:

| Field             | Type                       | Required | Semantics                                                                                  |
|-------------------|----------------------------|:--------:|--------------------------------------------------------------------------------------------|
| `kind`            | string                     | yes      | Progress kind. Open registry; initial values include `status`, `retry_backoff`, `file_mutation`, `token_cost_update`. |
| `label`           | string                     | no       | Short human-readable label.                                                                |
| `message`         | string                     | no       | Longer human-readable message.                                                             |
| `detail`          | string                     | no       | Multi-line detail body (e.g. error chain).                                                 |
| `iteration`       | u32                        | no       | Iteration counter for repeating progress (e.g. retry attempt).                             |
| `progress_pct`    | f32                        | no       | Progress percentage in `[0.0, 100.0]` when the producer can estimate one.                  |
| `retry`           | `UiRetryBackoff`           | no       | Typed retry-with-backoff block when `kind = "retry_backoff"`.                              |
| `file_mutation`   | `UiFileMutationNotice`     | no       | Typed file-mutation notice block when `kind = "file_mutation"`.                            |
| `token_cost`      | `UiTokenCostUpdate`        | no       | Typed token / cost update block when `kind = "token_cost_update"`.                         |
| `extra`           | object<string, JSON value> | no       | Forward-compat extension bag. Flattened into the parent object. Clients must ignore unknown keys. |

Replay behavior: lossless (durable ledger event). Every `progress/updated`
emitted on a connected WebSocket is committed to the per-session append-only
ledger before the wire frame is enqueued — both the
`LedgerStatusGateReporter` path (status / progress-gate variants via
`ledger.append_notification(...)` in
`crates/octos-cli/src/api/ui_protocol_alpha4_bridge.rs`) and the main
progress-mapping path (via `ledger.append_progress_from(...)` followed by
`send_ledger_event_durable(...)` in `crates/octos-cli/src/api/ui_protocol.rs`)
go through the durable write-ahead. Under per-connection backpressure the
frame may be dropped from the live socket — that drop surfaces as
`protocol/replay_lossy` (see below), and `session/open` replay returns the
missing `progress/updated` entries from the ledger ring (or the on-disk log
when the ring has rolled). Clients SHOULD treat the latest received
`progress/updated` of a given `metadata.kind` as authoritative for UI
rendering — newer values supersede older ones — but they can rely on every
emitted frame being available via replay until ledger retention rolls it off.

Backward compatibility: the kind has shipped since UPCR-2026-007's
`supported_notifications` list. The `extra` map is intentionally additive so
future progress kinds do not require a new UPCR per addition.

### `protocol/replay_lossy`

Source struct: `ReplayLossyEvent`
(`crates/octos-core/src/ui_protocol.rs`).

Wire signal that one or more durable notifications were dropped due to
per-connection backpressure. The client should diverge from its cursor and
rehydrate via REST snapshot or `session/open` replay. Carries the last known
durable cursor so the client can resume cleanly.

| Field                  | Type        | Required | Semantics                                                                                                |
|------------------------|-------------|:--------:|----------------------------------------------------------------------------------------------------------|
| `session_id`           | `SessionKey`| yes      | Owning session.                                                                                          |
| `dropped_count`        | u64         | yes      | Number of durable notifications dropped for this connection since the previous successful replay frame.  |
| `last_durable_cursor`  | `UiCursor`  | no       | Last cursor the server is confident the client durably observed. Absent only when the server has none.   |

Replay behavior: lossless (durable ledger event). The "lossy" in the name
refers to the condition the signal reports — that one or more *other*
durable notifications were dropped from the live socket under per-connection
backpressure — NOT to this signal's own durability. The reference server
emits the signal via `emit_replay_lossy_opportunistic` in
`crates/octos-cli/src/api/ui_protocol.rs`, which calls
`ledger.append_notification_from(...)` BEFORE attempting the wire send. The
append takes the same write-ahead-to-disk path as every other durable
notification (`ui_protocol_ledger.rs` `append` → `write_record_locked`), so a
`session/open` reconnect replays the `protocol/replay_lossy` itself alongside
the surrounding durable events. After observing the signal on either the
live socket or via replay, the client diverges from its local cursor and
rehydrates via `session/open` replay or REST snapshot.

Backward compatibility: the kind has shipped since UPCR-2026-007's
`supported_notifications` list. Spec § 9 ("Reconnect and Cursor Rules")
already requires the client to "diverge from its cursor and rehydrate" when
the server signals replay was lossy; this UPCR names the wire signal
explicitly.

## Migration

None. All ten (eight notification methods plus the two shared sub-structs
`UiContextState` / `UiProgressMetadata`) ship in production today.
This UPCR backfills documentation. No emitter, no field, no capability gate,
and no decoder behavior changes.

## Tests

- The spec's § 8 entries (added in this UPCR) match the field set serialized
  by `crates/octos-core/src/ui_protocol.rs`.
- `UI_PROTOCOL_NOTIFICATION_METHODS` lists every kind named here.
- A docs-contract test confirms each documented field is present on the
  matching struct (no spec-only fields, no struct-only undocumented fields
  for required-field rows).

## References

- `crates/octos-core/src/ui_protocol.rs`
- `crates/octos-cli/src/api/ui_protocol.rs`
- UPCR-2026-007 — capability advertisement payload that first listed the
  approval / progress / replay kinds.
- UPCR-2026-014 — `protocol/replay_lossy` companion to the durable ledger.
- UPCR-2026-019 — `turn/spawn_complete` passing reference in the M10
  spawn-only completion path.
- `docs/OCTOS_CONTEXT_MANAGER_GAP_CONTRACT.md` — M16 context-manager
  workstream that introduced the two `context/*` notifications.

## Explicit Non-Goals

- This UPCR does **not** add new fields, new kinds, or new capability gates.
- It does **not** modify the existing emitter code paths.
- It does **not** redefine which kinds are durable versus ephemeral; it only
  documents the existing behavior.
- It does **not** subsume the M16 context-manager contract — that workstream
  continues to own the `context.lifecycle.v1` capability and the lifecycle
  state machine.
