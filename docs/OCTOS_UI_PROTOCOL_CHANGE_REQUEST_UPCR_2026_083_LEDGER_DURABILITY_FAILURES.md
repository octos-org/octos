# UPCR-2026-083 — Ledger Durability Failures Without Wire Contract Changes

Status: proposed (implemented for review)
Task: T-0131
PR: #3000

## Decision record

This is a durability and recovery-error correction with no OUP method,
parameter, result field, notification, capability, cursor shape, or error-code
changes. Existing `session/open`, `session/hydrate`, replay, terminal, and
approval envelopes retain their current schemas.

A durable Ledger append now succeeds only after its log write succeeds. A write
failure is returned with operation, session, and candidate sequence context
before the event enters the in-memory ring, live broadcast, or durable-cursor
bookkeeping. Because an I/O error can be partial or ambiguous, the session is
then latched unavailable until restart reconciles the actual log; no same-process
retry can duplicate the candidate cursor. Terminal and approval send paths reuse
their existing lifecycle failure surface, and approval decisions persist before
the waiting runtime is woken. Auto-resolved decisions fail closed to denial when
their durable write fails. No success acknowledgement is emitted for an event
that was not logged.

Per-thread v2 allocation intents are persisted before the corresponding log
append. A watermark write failure rejects the emit before logging; a later log
failure may leave a safe sequence gap or completion barrier but never reuses or
reopens that allocation. Cold allocation reconciles watermarks against durable
replay, including after all retained rows rotate, and an unreadable watermark
returns an error instead of restarting from the bounded ring.

Cold recovery distinguishes genuinely absent history from existing but
unavailable history. Absent history may still start at sequence 1. A read or
recovery failure returns the existing `internal_error` RPC surface and rejects
subsequent append attempts instead of returning a successful empty snapshot or
reusing cursors. Malformed and unknown-version retained records are likewise
unavailable at the recovery/append boundary; known compatible legacy records
keep their existing recovery and cursor-preservation behavior. The projection
snapshot remains best-effort: its failure still falls back to full log replay
because the log remains the source of truth.

No RAM-only fallback, empty-history fallback, or new client compatibility
requirement is introduced.
