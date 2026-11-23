# UPCR-2026-REF9D0516 — IM bridge core on external turns (no protocol change)

Status: proposed
PR: #3334

## Decision record

T-0653 (`docs/IM_REMOTE_CONTROL.md` §14) adds `ServeImBridge`
(`api/ui_protocol_im_bridge.rs`). It is the core of an IM ingress: a direct
chat with a bot, bound by the operator to one existing octos chat, runs its
text messages as turns through `ExternalTurns` (UPCR-2026-RBB8E2A9A) and sends
each result back to the chat. Nothing in `octos serve` starts a bridge yet,
and its tests use a fake channel.

Nothing on the wire changes:

- No method, params, result, error code, message, `data`, capability,
  feature token or event shape is added, removed or altered.
- The bridge's turns are external turns on a ledger-only connection, so they
  write the same ledger rows an external client's turn writes. Only the
  message text reaches the model and the ledger. The channel, chat, sender
  and platform message id stay in the bridge.
- Internally, `BoundChat` and `BoundProfile` gain accessors, and
  `ExternalTurns` gains a way to wait for a turn's end and read its last
  reply from the ledger. A start that the chat refuses now removes the
  turn's `external_turns/` record, because a turn that never ran leaves
  nothing to reconcile.
- `ExternalTurns::start` checks again that the chat still exists before it
  admits a turn. `TurnConnection` gains `chats_only` (false unless
  overridden; true for `LedgerConnection`). A turn on such a connection
  whose session is no longer a chat when its runtime is built ends with
  the existing `runtime_unavailable` turn error, before any model call.
  WebSocket and stdio connections are not chats-only and behave as before.
- The bridge keeps each `/stop` decision in the profile's
  `im_bridge_stops/` directory until 24 hours after the `/stop`'s platform
  time; no bridge may accept a message for longer than that.
  `external_turns/` and `im_bridge_stops/` are server state, not protocol.

Cite this record elsewhere as `UPCR-2026-REF9D0516`.
