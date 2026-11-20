# UPCR-2026-R60B1A9DC — Peer purge State 1 authorized rescan

Status: proposed
PR: T-0398 (TBD)

## Decision record

`peer/purge` keeps its existing request, response and error shapes. Before the
first destructive erase step, the owning host now persists a versioned,
24-hour-bounded purge intent containing the originator, host-token digest, full
host/context bindings and residue scope. The intent is recovery state, not a
capability: no task or peer identifier becomes execution authority by being
written down.

When a retry cannot authorize from staged files, it may re-enter the same purge
executor only after independently proving the matching originator and host
token, any live host-route ownership, an unexpired intent, and either the same
staged binding generation or that generation's unfinalized slug tombstone.
Unknown or forged identifiers, expired records and finalized/later generations
fail closed; expired intents are removed. The active owner performs the bounded
rescan through the existing in-flight guard, so concurrent retries coalesce and
a closed WebSocket does not make the broker wait for a nonexistent active turn.

Successful completion still writes the token tombstone and finalizes the slug
record only after the erase, then removes the intent. Partial failure leaves it
for an authorized retry. State 2 containment refusal, native worker semantics
and all wire-level compatibility contracts are unchanged by
`UPCR-2026-R60B1A9DC`.
