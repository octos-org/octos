# UPCR-2026-RBB8E2A9A — Ledger-only turn runner and external turn entry (no protocol change)

Status: proposed
PR: #3325

## Decision record

T-0637 (`docs/IM_REMOTE_CONTROL.md` §14) lets a turn run without a socket.
`run_standalone_turn` and the send helpers it calls now deliver through a
`TurnConnection` (`api/ui_protocol_turn_connection.rs`) instead of a
`WsConnection`. A `WsConnection` is one implementation and delivers exactly
as before. The other, `LedgerConnection`, has no client: a turn on it still
appends every durable event to the session ledger, and its readers get them
there. An internal entry, `ExternalTurns` (`api/ui_protocol_external_turns.rs`),
starts, interrupts and answers turns on it through `SessionTurnService`, for
a later IM bridge. Nothing in `octos serve` calls it yet.

Nothing on the wire changes:

- Methods, params, results, error codes, messages and `data`, capabilities,
  feature negotiation and event shapes are untouched. The entry adds no
  method or event; its turns write the same ledger rows an external
  client's turn writes.
- For a WebSocket or stdio connection every send helper does what it did:
  the same ledger appends, origin tags, capability filtering, direct sends,
  backpressure and failure latching, in the same order. Each
  `TurnConnection` method of `WsConnection` calls the method it replaces.
- `ConnectionUiFeatures` becomes `pub(crate)` so the trait can name it; its
  fields stay private.
- The entry records the id of each turn it starts in the profile's data
  directory (`external_turns/`) before the turn runs. That file is server
  state, not protocol.

No capability, feature token, method, error shape, or storage format is
added, removed, or altered. Cite this record elsewhere as
`UPCR-2026-RBB8E2A9A`.
