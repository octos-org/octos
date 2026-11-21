# UPCR-2026-R60B1A9DC — Peer purge State 1 authorized rescan

Status: proposed
PR: octos#3328

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
staged binding generation or that generation's unfinalized slug tombstone. A
staged binding that differs from the intent is always a later generation and is
never erased, even if the slug record is still unfinalized. Unknown or forged
identifiers, expired records and finalized/later generations fail closed;
expired intents are removed. The active owner performs the bounded rescan
through the existing in-flight guard, so concurrent retries coalesce and a
closed WebSocket does not make the broker wait for a nonexistent active turn.

Successful completion still writes the token tombstone and finalizes the slug
record only after the erase, then removes the intent. A retry that observes the
token tombstone after a crash also finishes any missing slug finalization and
intent cleanup for the same authorized generation, without repeating destructive
erasure. Partial failure leaves the intent for an authorized retry. Expiry is a
hard deletion-authority bound, not a promise of eventual cleanup: after 24
hours the intent is removed and any still-unerasable residue remains closed and
refused for operator inspection rather than being deleted by an unbounded stale
credential. State 2 containment refusal, native worker semantics and all
wire-level compatibility contracts are unchanged by
`UPCR-2026-R60B1A9DC`.
