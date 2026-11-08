# UPCR-2026-RF6720565 — Deleted-session reads and idle interrupt use unknown_session

Status: proposed (implemented for review)
Task: T-0461
Capability: none; reuses the existing `unknown_session` typed error

## Decision record

After `session/delete`, later `session/open` and `session/status/read` for
that deleted incarnation return `unknown_session`. An idle `turn/interrupt`
whose session has a deletion tombstone returns the same error; an active turn
still follows the existing interrupt/terminal path so cancellation is not
lost. Unknown, never-created sessions keep their previous status/interrupt
behavior.

The lifecycle tombstone is read without creating a lock, session, or
discovery file. First-open discovery refuses a deleted incarnation before
replacing cached history or creating its marker; the existing locked rewrite
barrier remains as the concurrent-delete backstop. Storage and lifecycle
read/write failures other than the exact deleted-session refusal remain
`runtime_unavailable`; they are not hidden as not-found.

Validation: T-0461's real RPC-handler regression covers delete then open,
status and interrupt, unchanged discovery bytes, no resurrected record, and
foreign-profile isolation. Existing session, turn, and transport suites run
on the PR. Cite this record as `UPCR-2026-RF6720565`.
