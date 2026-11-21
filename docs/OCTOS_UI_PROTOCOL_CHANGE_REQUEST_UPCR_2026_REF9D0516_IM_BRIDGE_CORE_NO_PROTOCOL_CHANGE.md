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
- Internally, `BoundChat` gains a profile accessor, and `ExternalTurns`
  gains a way to wait for a turn's end and read its last reply from the
  ledger. A start that the chat refuses now removes the turn's
  `external_turns/` record, because a turn that never ran leaves nothing to
  reconcile. That file is server state, not protocol.

Cite this record elsewhere as `UPCR-2026-REF9D0516`.
