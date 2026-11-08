# UPCR-2026-R8FDDC88D — session/open live-forwarder quota

Status: proposed
PR: TBD

## Decision record

**Problem (R-05, robust audit):** every `session/open` installed a
per-session live forwarder task, a map entry and a ledger subscription
with NO per-connection or process-wide quota. The ledger's own
1024-session LRU never evicts a forwarder whose broadcast receiver is
still alive, so a client opening distinct sessions grew tasks, memory
and disk without bound (audit repro: 1025 opens → 1025 forwarders while
`sessions_active` capped at 1024).

**Decision:** `session/open` passes an admission check before doing any
open work:

- Per-connection cap: 128 distinct live forwarders (far past any real
  client; reopens of an already-open session replace the existing
  forwarder and consume no slot). Finished forwarder tasks are reaped
  at admission so the cap reflects live work, not stale entries.
- Process-wide cap: 4096 forwarder slots via a CAS counter; the slot is
  an RAII permit held by the forwarder task and released when it ends
  (including abort).
- Refusal is a JSON-RPC error with `data.kind =
  "session_open_limit_reached"` (and `scope: "connection" | "global"`);
  the code is the existing `internal_error` (-32603) so no new wire
  code is minted. The refused open drops its fresh broadcast receiver
  and prunes the idle subscriber slot, mirroring the existing
  scope-error path.
- `session/delete` now retires this connection's forwarder for the
  deleted session and prunes the broadcast subscriber, so a delete
  actually frees resources instead of leaving a task parked on a dead
  channel until reconnect.

**Wire compatibility:** one new refusal `data.kind` value
(`session_open_limit_reached`) on `session/open`; no message, field or
codec changes. Well-behaved clients (a handful of open sessions) are
unaffected.

Cite this record elsewhere as `UPCR-2026-R8FDDC88D`.
