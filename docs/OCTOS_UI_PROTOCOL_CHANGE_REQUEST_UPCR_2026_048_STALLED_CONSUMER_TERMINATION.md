# UPCR-2026-048: cooperative turn-drain delivery with a budgeted terminal shed

- **Request id**: UPCR-2026-048
- **Date**: 2026-10-14
- **Status**: implemented
- **Implements**: the client-visible half of #2807 (turn termination must not
  park on a stalled consumer)
- **Sibling records**: delivery mechanics reuse the cooperative stdio probes
  `send_durable_offloaded` already used for durable notifications; the
  termination guarantee is new.

## Problem

A turn ends only when the drain loop emits a terminal frame (`turn_completed`
or `turn/error`). Both the mid-turn forwards and the agent task's terminal
event were unbounded blocking sends: a consumer that stalls — a stdio host
that stops reading its queue, so the drain loop parks inside a forward while
holding the progress channel full — parked the turn forever. The client sees
a turn that never terminates: no terminal frame, no error, and the session's
turn slot stays occupied. #2803 fixed one instance of this shape for the
progress-flush barrier; the turn-loop completion path itself had no bound.

## Contract

1. **Cooperative mid-turn forwards.** Every forward the turn drain loop and
   the agent task make — agent record updates, `task/output/delta`, mapped
   notifications, approval/structured-question drains at interrupt, leftover
   steer settlement — parks cooperatively on the stdio lane (non-blocking
   probe then async sleep, mirroring `send_durable_offloaded`) instead of a
   blocking `SyncSender::send` that pins an executor worker. The WS lane
   delegates to the existing sync senders unchanged, keeping the #922.2
   latched-failure mapping byte-identical. Frame shapes, ordering, and the
   durable/ephemeral/lifecycle ledger classification are unchanged.
2. **Budgeted terminal event.** The agent task's terminal event (the
   `done`/`error` JSON it queues on the turn's progress channel) gives up
   after 10 seconds (`TERMINAL_SEND_BUDGET`) instead of parking: the shed is
   counted into the turn's drop counter, so the turn-boundary
   `protocol/replay_lossy` discloses the loss, and one
   `ws.send.drop.backpressure{method=turn_terminal}` metrics counter plus a
   `warn` log names the shed. On the delivered path nothing changes.
3. **Guaranteed termination.** The drain loop gains a completion signal: the
   agent task holds a completion sender for its whole body, so the loop wakes
   the moment the body ends — whichever exit it takes. The signal is biased
   after progress, so a queued terminal event still delivers first; the arm
   only ends a turn whose terminal event never made it (shed per §2, or the
   task panicked). That path closes the turn with a `turn/error` frame whose
   code is `terminal_lost` and mirrors the error arm's closing work (peer
   result, `replay_lossy` flush, turn-end hooks, steer settlement) instead of
   parking in receive forever.
4. **No schema change.** No new method, notification type, payload field,
   enum variant, or capability bit. The only new wire value is the
   `terminal_lost` code string on the existing `turn/error` frame, listed
   here so clients can distinguish it from LLM-failure codes.

## Why this record exists

The implementation edits `crates/octos-cli/src/api/ui_protocol_transport.rs`
and its test modules, and the completeness gate
(`scripts/check-ui-protocol-upcr.sh`) treats any edit to a `ui_protocol_*.rs`
file as protocol-visible. The delivery mechanics deliberately add no protocol
surface, but §2–§3 change client-observable behavior — a turn that used to
hang forever now terminates (possibly with `terminal_lost` and a
`replay_lossy` disclosure) — so governance records the behavioral contract.

## Non-goals

- Retransmitting a shed terminal event from durable storage: the agent task's
  `done`/`error` JSON is not durable; the `replay_lossy` disclosure is the
  contract, matching the #2803 flush deadline.
- Bounding the WS lane's sends at the transport layer: the WS lane already
  delegates to the existing sync semantics; only the cooperative parking of
  the stdio lane changes.
- A `turn/state` sub-state for the shed terminal: the turn ends in the same
  `Errored`/`Interrupted` terminal set as before; `terminal_lost` plus the
  `replay_lossy` disclosure is the whole story.
- The post-loop router-status sync send and the other send surfaces flagged
  in the #2810 review are follow-up work (#2811), not part of this record.
