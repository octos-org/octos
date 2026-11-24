# UPCR-2026-R9EE04597 — A turn's peer handoffs are not counted or capped

Status: proposed

## Decision record

No wire shape changes. `UPCR-2026-R9EE04597` removes a server-side limit: the
fifth `peer_handoff` call of one turn was refused with `peer handoff limit
reached for this turn (4)`. A turn may now hand off as many peers as it names.

Each handoff is still staged or refused on its own, by the rules that already
applied to it: the peer's name must be new, the brief must fit 64 KB, a declared
required input must be there. A refused handoff does not affect the ones before
or after it.

Unchanged:

- A peer session is never offered `peer_handoff`, so peers cannot hand off in
  turn (depth 1).
- `peer/prepare` stages a fleet of 1–8 peers.
- `peer_handoff` calls of one turn run one at a time.

What a client observes:

- One turn of a session may be followed by more than four `peer/staged`
  notifications. Each carries the same fields as before, is durable, and is
  replayed on reconnect; a client opens each staged session as it did.
- The model no longer receives the limit error.

What no longer bounds a turn: the number of peer sessions it starts. Each staged
peer runs its own turns and spends its own tokens; a peer's `token_budget` when one
was set, and a goal's budget where the peers belong to one, are what bound that
spend.

No protocol method, notification, field, session or turn identifier, or
capability is added or reinterpreted by `UPCR-2026-R9EE04597`.
