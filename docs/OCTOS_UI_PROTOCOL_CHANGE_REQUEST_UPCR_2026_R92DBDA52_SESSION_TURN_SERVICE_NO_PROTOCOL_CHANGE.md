# UPCR-2026-R92DBDA52 — SessionTurnService extraction (no protocol change)

Status: proposed
PR: TBD

## Decision record

T-0636 (`docs/IM_REMOTE_CONTROL.md` §14) moves the core of four UI Protocol
handlers — `session/open`, `turn/start`, `turn/interrupt` and
`approval/respond` — out of the WebSocket transport into an internal
`SessionTurnService` (`api/ui_protocol_session_turn_service.rs`), so a
later ingress can drive the same sessions without a socket. The WebSocket and
stdio handlers now call it and put its outcome on the wire. Nothing on the
wire changes:

- Methods, params, results, error codes, messages and `data`, capabilities
  and feature negotiation are untouched; scope errors still go out through
  the same path, including the 1008 close on an authenticated auth-scope
  violation.
- The order of effects is kept: `approval/decided` is appended and
  delivered before the `approval/respond` result, the audit entry after it;
  a `turn/start` accept is answered before the turn may run, an accept
  that cannot be delivered still aborts the turn, and the admission markers
  `turn/state/get` reads stay up until the answer; the `session/open`
  subscription, replay and live forwarder keep their order.
- Ownership, host-managed external and work-secret ingress checks, the turn
  registries, the approval store and ledger appends are the same code.

Admitted turns still run through `run_standalone_turn` on the requesting
connection. No capability, feature token, method, error shape, or storage
format is added, removed, or altered. Cite this record elsewhere as
`UPCR-2026-R92DBDA52`.
