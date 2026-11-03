# UPCR-2026-RFAEE24B4 — Turn Id Reuse Recovery

Status: proposed (implemented for review)

## Decision record

Restart reconciliation must treat each `TurnStarted` as opening a new lifecycle
generation, even when its wire `turn_id` was used by an earlier terminal turn.
A `TurnCompleted` or `TurnError` closes only the latest preceding generation;
it must not close a later generation that has started but has no terminal of
its own. After process death, recovery appends one `orphaned_by_restart` error
for that latest unclosed generation. A subsequent recovery sees the synthesized
terminal and appends nothing further.

This is a recovery correction to existing lifecycle events. No method, DTO,
capability, lifecycle state, or persistence schema changes. Existing sessions
with unique turn ids retain their behavior. Older records need no generation
field: event order supplies the boundary. A reused generation with no terminal
of its own is honestly degraded to a recovery-synthesized error, so the earlier
terminal state and `completed_at_durable` value cannot be inherited, and no
durable execution end time is fabricated. The synthesized terminal retains the
existing recovery-time `completed_at` compatibility behavior and omits
`completed_at_durable`.
