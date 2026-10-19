# UPCR-2026-050: turn-task delivery residue — the last sync durable sends, unified lifecycle error shapes, and real usage for shed turns

- **Request id**: UPCR-2026-050
- **Date**: 2026-10-14
- **Status**: implemented
- **Implements**: #2811, the four residue faces the #2810 review rounds
  verified as remaining after UPCR-2026-048
- **Sibling records**: delivery mechanics reuse the cooperative stdio
  probes UPCR-2026-048 introduced; usage accounting reuses the #1969
  interrupted-turn fallback source.

## Problem

UPCR-2026-048 converted the turn drain loop's mid-turn forwards, terminal
gate, and completion arm to cooperative delivery, but four residue faces
kept the old behavior:

1. The post-loop `router/status` snapshot still enqueued through the
   blocking durable send — the last blocking `SyncSender::send` in the
   turn-closing sequence, pinning an executor worker whenever the stdio
   queue was full.
2. The approval-cancel path after a failed `approval/requested` send still
   enqueued its `approval/cancelled` frame through the blocking durable
   send, from async call sites.
3. The offloaded lifecycle lane reported an already-latched connection as
   a bare `FatalClosed`, while the sync `send_lifecycle` twin maps the
   same state to `LifecycleFailure("connection already latched as
   failed")` (#924 BLOCK 2) — the twins' documented "identical error
   semantics" was not true for callers that inspect the variant.
4. A turn whose terminal event was shed past the send budget recorded 0
   usage, even though the live token tracker accumulated the real partial
   spend while the turn ran (the interrupt path already falls back to that
   source).

## Contract

1. **Cooperative post-loop and cancel delivery.** The post-loop
   `router/status` snapshot and the approval-cancel
   `approval/cancelled` frame enqueue through the offloaded durable lane:
   on a full stdio queue the turn task parks cooperatively (non-blocking
   probe then async sleep) instead of pinning an executor worker, and
   delivers once the host reader drains. The WS lane keeps its
   non-blocking semantics unchanged. Frame shapes, ledger classification,
   ordering, and the error handling at every call site are unchanged.
   Two deliberate nuances: the cancel path's reachable error set (WS
   backpressure, or a dead/latched stdio lane) returns without waiting,
   so its offloaded enqueue is consistency with the calling arms rather
   than a new wait path; and a cooperative park is cancellable at its
   await, so an abort of the turn task mid-park skips the enqueue where
   the old blocking send would have completed it — the ledger row is
   already appended in that window and reconnect-replay delivers it, the
   same recovery every UPCR-2026-048 offloaded send relies on.
2. **Unified latched error shape.** The offloaded lifecycle lane maps an
   already-latched connection to the same
   `LifecycleFailure("connection already latched as failed")` the sync
   `send_lifecycle` twin reports. The sync `send_durable` twin keeps its
   `FatalClosed` mapping — the twins are unified per family, not across
   families.
3. **Real usage for shed turns.** The `terminal_lost` arm folds the live
   token tracker's input+output spend into `final_tokens_consumed` before
   the peer-result write and the post-loop goal accounting, so a shed
   turn charges what it actually burned instead of 0. The fold uses the
   same fallback source (and the same input/output-only accounting, no
   cache) as the interrupt path's `interrupted_goal_charge`; the post-loop
   charge sees the nonzero fold and keeps it, so nothing double-counts.
   The `terminal_lost` wire frame itself is unchanged — it still carries
   no token usage, because the shed event's usage payload is gone; the
   correction is in the accounting surfaces only.

## Why this record exists

The implementation edits `crates/octos-cli/src/api/ui_protocol_transport.rs`
and its test modules, and the completeness gate
(`scripts/check-ui-protocol-upcr.sh`) treats any edit to a `ui_protocol_*.rs`
file as protocol-visible. §1–§2 add no protocol surface and change no
client-visible frame; §3 changes which number the internal usage
accounting surfaces record for a shed turn. The delivery and error-shape
changes extend UPCR-2026-048's cooperative-delivery contract to the
turn-closing sequence (the drain-loop body itself was already converted),
so governance records them alongside it rather than as a new behavioral
surface.

## Non-goals

- The agent task's own approval/question send surface: `request_approval`
  still enqueues `approval/requested`, `approval/decided`, and
  `approval/auto_resolved` through the sync blocking durable send, and
  `request_user_question` does the same for its frames. A stalled stdio
  host that filled the queue can park the agent task on those sends with
  no send budget — which also suppresses the UPCR-2026-048 completion
  signal, so the turn cannot even reach `terminal_lost`. That is the same
  hazard family, but a different task with its own delivery-semantics
  surface (the closed/failed arms' keep-pending-vs-cancel behavior), so
  it is recorded as follow-up work (#2815) rather than folded into this
  record.
- Converting the remaining pre-loop sync durable sends inside
  `run_standalone_turn` (the pre-turn context-lifecycle rows, the voice
  no-speech progress rows, the turn/started lifecycle send, and the peer
  runtime's sync emit closures): those run before the drain loop or on
  non-turn tasks, were not part of the #2810 review's residue list, and
  the closure-shaped ones cannot adopt an async lane without a redesign.
- Bounding the post-loop router/status or cancel sends with a shed
  budget: like the durable forwards, they wait for capacity instead of
  dropping — a status pill or cancel frame is worth the wait once the
  parking is cooperative.
- Attaching the recovered usage to the `terminal_lost` wire frame: the
  frame's shape is fixed by UPCR-2026-048 §4 (no new fields).
