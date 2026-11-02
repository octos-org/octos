# UPCR-2026-078 — Durable Turn Terminal Time

Status: proposed (implemented for review)
Task: T-0196

## Decision record

`session/hydrate` and `turn/state/get` must report the time a turn actually
reached a terminal state, not the later time at which a snapshot is projected.
This is a timestamp-source correction to existing `turns.started_at` and
`turns.completed_at`; no method, DTO field, capability, or lifecycle state is
added.

Each ledger append captures one wall-clock `recorded_at` and stores it in the
existing disk record and projection snapshot as an additive optional field.
`TurnCompleted` and `TurnError` projections read that value for `completed_at`.
The same value is used by full and turn-scoped reads, repeated hydration, and
cold recovery. Active turns have no terminal event and therefore no
`completed_at`.

Legacy records without `recorded_at` retain `completed_at: null`; projection
must not synthesize a replacement from `Utc::now()` or from a message's
persisted time. The on-disk record and snapshot versions remain unchanged:
old readers ignore the additive field, and old records deserialize with no
terminal time. Existing authorization, cursor, and turn-state semantics are
unchanged.
