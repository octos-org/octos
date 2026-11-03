# UPCR-2026-R6A29286F — First-Turn Session Discovery Without Wire Contract Changes

Status: proposed (implemented for review)
Task: T-0122
PR: #2959

## Decision record

This is a record-only change. It adds no OUP method, parameter, result field,
notification, capability, or error variant. Existing `session/open`,
`turn/start`, `session/list`, `session/delete`, and replay wire shapes are
unchanged.

A successful `session/open` may now leave a metadata-only durable session
record before the first transcript row exists. After a first turn is admitted,
its existing title/activity metadata may be recorded before an approval wait.
Consequently, an existing `session/list` call can return the session while its
first turn is still running instead of only after terminal completion. The row
uses the existing session id, profile/cwd scope, title, timestamps, and
`active_turn` projection; no separate catalog or SessionRef API is introduced.

## Authorization and persistence compatibility

The record is written only after the existing replay-cursor validation and
profile/cwd authorization checks, through the already-resolved owning
`SessionManager`. Rejected opens create nothing, cross-profile/external scopes
are not widened, and deleted sessions are not recreated from cached history.

A durable internal first-open marker keeps subsequent Manager and
SessionHandle writes on one canonical per-user layout. Unmarked legacy flat or
mixed-store sessions retain their previous behavior. The marker and metadata
are storage implementation details, not wire fields or a migration of legacy
transcripts.

Existing terminal turn commits remain responsible for transcript rows. Prompt
notes do not append a user message early, and replay ordering is unchanged.
