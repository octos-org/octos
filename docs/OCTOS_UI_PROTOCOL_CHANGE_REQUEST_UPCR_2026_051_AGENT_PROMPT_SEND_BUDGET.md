# UPCR-2026-051: agent-task prompt sends — budgeted cooperative delivery for approvals and questions

- **Request id**: UPCR-2026-051
- **Date**: 2026-10-15
- **Status**: implemented
- **Implements**: #2815, the agent-task surface UPCR-2026-050 recorded as
  its first non-goal
- **Sibling records**: delivery mechanics reuse the cooperative stdio
  probes UPCR-2026-048 introduced; the budget model reuses
  UPCR-2026-048's terminal send budget (and, through it,
  `progress_delivery::FLUSH_DEADLINE`).

## Problem

UPCR-2026-048 converted the turn drain loop's forwards, terminal gate,
and completion arm to cooperative delivery, and UPCR-2026-050 moved the
turn-closing sequence to the same lane — but the agent task's own
approval/question send surface still enqueued through the sync blocking
durable send:

- `request_approval`: `approval/auto_resolved`, `approval/decided`
  (both in the scope-policy auto-resolve short circuit), and
  `approval/requested`
- `request_user_question`: its `user_question/requested` frame

On `octos serve --stdio` with a host that stops reading, the stdio queue
fills (exactly the stalled-consumer state UPCR-2026-048's budgeted shed
exists for) and the blocking `SyncSender::send` parks the **agent task**:

1. an executor worker is pinned for as long as the host stalls;
2. the agent task holds its completion sender, so the drain loop never
   observes `AgentFinished` — `terminal_lost` and the terminal send
   budget never fire, and the turn hangs forever (the #2803/#2807 hang
   shape, one task over);
3. unlike the terminal gate there is no send budget on this path at all.

## Contract

1. **Cooperative prompt delivery.** The four prompt sends enqueue through
   the offloaded durable lane: on a full stdio queue the agent task parks
   cooperatively (non-blocking probe then async sleep) instead of pinning
   an executor worker, and delivers once the host reader drains. The WS
   lane delegates to the sync durable send unchanged. The error taxonomy
   the arms match on is unchanged: the stdio lane produces the same
   `Closed`/`FatalClosed` the blocking send produced (so `approval/requested`
   keeps its keep-pending-for-reconnect behavior and `user_question/requested`
   keeps its degrade-to-fallback behavior), and the WS lane can still
   produce `BackpressureDrop`.
2. **Budgeted shed.** The prompt sends are bounded by `PROMPT_SEND_BUDGET`
   (10s — the same stall model and budget as the terminal send budget).
   Past the budget the send sheds as `BackpressureDrop`, the same shape a
   full WS queue produces instantly, so the callers' existing fail-closed
   arms run: a shed `approval/requested` cancels the pending approval and
   denies the tool call, a shed `user_question/requested` cancels the
   pending question and degrades to the `Unsupported` fallback, and the
   fire-and-forget auto-resolve frames shed without touching their
   already-recorded decisions. The shed counts through the same
   accounting as a WS backpressure drop — the connection's drop
   accumulator (surfaced as `protocol/replay_lossy`), the
   `ws.send.drop.backpressure` counter tagged with the frame's method,
   and a warning with the budget — so a shed prompt is disclosed, never
   silent. A concurrent lifecycle failure that latches the connection
   during the park is reported as `FatalClosed` (the keep-pending arm's
   trigger) rather than masked as backpressure. Ledger rows are appended
   before the send (the durable path's existing prep in
   `prepare_durable_send`), so a shed frame still reaches a reconnecting
   client via replay. The budget is the terminal budget's stall model,
   not a host-liveness probe: a live host that reads slower than the
   turn produces progress can hold the queue saturated past the budget
   and forfeits the live prompt the same way a stalled host does. The WS
   lane already forfeits it instantly under the same saturation, the
   ledger row still replays, and the shed is disclosed — the bounded
   turn is the point of the surface.
3. **The cancel follow-up rides the same budget.** The
   `approval/cancelled` frame sent after a failed `approval/requested`
   (UPCR-2026-050 §1's cooperative enqueue) moves from the unbudgeted
   offloaded lane to the budgeted one: a shed prompt left the very same
   queue full, so an unbudgeted cooperative park in the cancel path would
   move the hang one frame later. The `approval/cancelled` ledger row is
   appended by `prepare_durable_send` before the enqueue attempt, so a
   shed cancel frame is still delivered by replay.

## Why this record exists

The implementation edits `crates/octos-cli/src/api/ui_protocol_transport.rs`
and its test modules, and the completeness gate
(`scripts/check-ui-protocol-upcr.sh`) treats any edit to a `ui_protocol_*.rs`
file as protocol-visible. No client-visible frame shape, method, ledger
classification, or ordering changes; no error variant the arms match on is
added or removed (`BackpressureDrop` becomes reachable on the stdio lane
only through the new budgeted path, where the arms already handle it
exactly as they do on the WS lane). The change extends UPCR-2026-048's
budgeted-shed contract to the agent task's prompt surface — the surface
UPCR-2026-050 explicitly deferred here — so governance records it in the
same family rather than as a new behavioral surface.

## Non-goals

- The await-after-send lifecycle is untouched: `request_approval` still
  parks on its oneshot until the client decides, with the #1449 waiter
  guards and the interrupt drain as backstops. The budget bounds only the
  enqueue — the client's answer window is a product semantic, not a
  delivery stall. `Closed`/`FatalClosed` keep-pending semantics are
  unchanged.
- The drain loop's mid-turn forwards stay unbudgeted (UPCR-2026-048
  semantics: a progress forward waits for capacity; bounding that wait is
  the flush deadline's job, not the forward's).
- The M9 fixture turn's scripted sends (`run_m9_fixture_turn`, including
  its `approval/requested` demo) stay on the sync lane: a deterministic
  demo surface with its own choreography, not the agent task.
- Sync durable sends on other non-turn surfaces (manual compaction
  results, background-task terminal mirrors) are unchanged: they are not
  on the agent task and were not part of #2815.
