# UPCR-2026-R559FDAB9 — Native peer reopen and fork (octos#2026)

Status: proposed
PR: #3333

## Decision record

octos#2026 / T-0411. Adds two master-side tools, `peer_reopen` and
`peer_fork`, completing the native-peer durable-lifecycle story. No wire
schema change, no new RPC method, no notification kind change; the tools are
registered only on peer-handoff-capable (depth-1, non-peer) sessions, exactly
like `peer_close`.

- `peer_reopen` clears a CLOSED peer's durable `closed` marker under
  originator-only authorization, resuming the SAME `peer-<slug>` identity
  with briefs, dated result versions, parked rows and workspace binding
  preserved. Already-committed results are not replayed; the close marker
  remains the single lifecycle authority, so crash/restart and the live view
  agree and reopening an open peer is refused. Host-owned app peers are
  refused (their app owns the lifetime, UPCR-2026-034).
- `peer_fork` mints a NEW peer identity with an explicit narrow inheritance:
  brief + workspace binding + same originator. In-flight tasks, approvals,
  task-id bindings and result history are NOT carried over; the source is
  never modified.

Compatibility: older clients/masters simply never call the new tools;
existing peer lifecycle behavior is unchanged.
