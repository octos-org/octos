# UPCR-2026-047: budgeted `monitor/expired` ledger admission per owning session

- **Request id**: UPCR-2026-047
- **Date**: 2026-10-13
- **Status**: implemented
- **Implements**: the client-visible half of #2498 (the durable expiry
  transition itself is unchanged)
- **Sibling records**: the admission state machine is the one #2019 introduced
  for the `background/activity` human sink (`human_events::cap_admit`), now
  shared.

## Problem

`monitor/expired` is a typed lifecycle notification: the server appends one
frame to the owning session's ledger for every monitor that transitions
active→expired. The client-side tap that appends those frames feeds a
process-global, bounded queue (512, `BACKGROUND_ACTIVITY_QUEUE_CAPACITY`)
while its producer — the expiry paths — is not bounded: one session that
stacks monitors and re-arms them in a loop can enqueue expiry frames
unboundedly and crowd other sessions' frames out of the shared queue, so
unrelated sessions silently lose their expiry notifications. #2019 fixed the
identical shape for the `background/activity` human sink with a per-origin
rolling-window budget; the `monitor/expired` tap had no equivalent.

## Contract

1. **Admission budget.** Frames handed to the sink are admitted per owning
   session under a rolling-window budget: at most 40 frames per session per
   60-second window (`human_events::cap_admit`, the #2019 state machine).
   40 sits far above legitimate traffic — expiry is a once-per
   active→expired-transition lifecycle event, `MAX_MONITORS_PER_SESSION` is
   16, and the re-arm paths refuse past-due monitors (#2367) — so the budget
   only binds under a pathological storm.
2. **Suppression visibility.** No in-band protocol surface is added: the
   wire shape stays a typed lifecycle frame, so there is no marker row.
   The first over-budget frame per window raises ONE `warn` log naming the
   session, the monitor, and the suppression count, and every suppressed
   frame increments the `ws.monitor_expired.budget_drop` metrics counter.
   When a later window admits again, the carried-over backlog is not
   re-reported in-band; the counter already covers the total.
3. **Durable behavior unchanged.** The monitor's durable status transition
   and the session note are emitted regardless of admission; only the
   client-side ledger tap is bounded. A suppressed expiry still expired,
   and the model-visible session note still says so.

## Why this record exists

The implementation's production-wiring test lives in
`crates/octos-cli/src/api/ui_protocol_tests.rs` — it pins the
ledger-visible surface (45 real watcher-site active→expired transitions
land exactly the budgeted number of ledger frames) — and the completeness
gate (`scripts/check-ui-protocol-upcr.sh`) treats any edit to a
`ui_protocol_*.rs` file as protocol-visible. The change deliberately adds
no method, notification, payload, enum variant, capability bit, or protocol
identifier; this record documents the behavioral budget on an existing
stream so governance stays honest about the client-visible change.

## Non-goals

- In-band truncation markers or a typed budget frame for `monitor/expired`:
  the stream is a lifecycle event, not a free-text stream; #2498 asks for
  exactly the warn-level drop counter this record specifies.
- Per-monitor budgets (16 × 40 per session could still fill the 512-slot
  queue): the budget keys on the owning session, which is the stricter
  posture. The ≥13-session aggregate-storm exposure is the same shared-queue
  posture the human sink accepted (#2019).
- Backpressure on the durable transition path: admission never blocks,
  delays, or fails the expiry transition.
