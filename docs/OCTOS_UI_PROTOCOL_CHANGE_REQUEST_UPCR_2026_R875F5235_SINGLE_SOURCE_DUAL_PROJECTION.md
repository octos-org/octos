# UPCR-2026-R875F5235 — Single-source dual projection: durable sink gaps and wake authority

Status: proposed
PR: TBD

## Decision record

octos#2021 / T-0410. The background-activity HUMAN sink and the agent wake
remain two READ-ONLY projections of the same authoritative session ledger —
no second bus is introduced. Two additive, wire-compatible deltas:

1. A sink queue-full drop now appends a rate-limited (60 s per session)
   marker row to the SAME durable ledger the drain writes. The marker reuses
   the existing `BackgroundActivityEvent` shape with `suppressed: true` and
   `dropped_count` = drops since the previous marker, so replay after
   reconnect shows the hole instead of a silently complete-looking stream.
   No schema change, no new method, no new notification kind.
2. The peer awaiting-input wake prompt now ends with the uniform
   `SYSTEM_EVENT_AUTHORITY_NOTICE` (an automated system event, not a user
   message, NOT an approval or confirmation of anything pending). Prompt text
   on the agent projection only; no wire format, capability, or permission
   changes. Consent remains exclusively on the existing approval/
   `peer_respond` channel.

Compatibility: clients that already render `suppressed`/`dropped_count`
(cap-marker support) render the new gap marker identically; older clients see
an ordinary background-activity row. Replay order and cursor semantics are
unchanged and pinned by the new dual-projection test.
