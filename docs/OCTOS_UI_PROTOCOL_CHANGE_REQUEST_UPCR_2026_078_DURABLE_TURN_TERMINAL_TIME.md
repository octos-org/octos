# UPCR-2026-078 — Durable Turn Terminal Time

Status: proposed (implemented for review)

## Decision record

`session/hydrate` and `turn/state/get` must report the time a turn actually
reached a terminal state, not the later time at which a snapshot is projected.
This is a timestamp-source correction to existing `turns.started_at` and
`turns.completed_at`. No method, capability, or lifecycle state is added. The
per-turn optional boolean `completed_at_durable` is added to hydrate and
turn-state projections so clients can distinguish trustworthy timestamps from
legacy or older-core snapshot values.

Each ledger append captures one wall-clock `recorded_at` and stores it in the
existing disk record and projection snapshot as an additive optional field.
`TurnCompleted` and `TurnError` projections read that value for `completed_at`.
The same value is used by full and turn-scoped reads, repeated hydration, and
cold recovery. Active turns have no terminal event and therefore no
`completed_at`.

`completed_at_durable` is `true` only when that projection's `completed_at` was
read from a durable `recorded_at` on an original execution terminal event. A
recovery-synthesized `orphaned_by_restart` terminal retains its recovery
`completed_at` for compatibility but omits the boolean on first and repeated
cold recovery. Active turns, including a reused turn id with an older terminal
record, omit both `completed_at` and `completed_at_durable`; legacy records and
older cores also omit the boolean, so clients must not infer durability from
`completed_at` alone. Clients may display elapsed time only when it is `true` and
start/end are valid. Projection must not synthesize a replacement from
`Utc::now()` or from a message's persisted time. The on-disk record and snapshot
versions remain unchanged: old readers ignore the additive fields, and old
records deserialize with no terminal time or durability marker. Existing
authorization, cursor, and turn-state semantics are unchanged.
