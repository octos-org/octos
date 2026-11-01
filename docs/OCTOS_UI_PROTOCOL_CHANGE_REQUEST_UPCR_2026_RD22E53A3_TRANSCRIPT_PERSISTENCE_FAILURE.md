# UPCR-2026-RD22E53A3 — OUP transcript persistence failure terminals

Status: proposed (implemented for review)
Task: T-0176 / D-04
Capability: none (existing turn error and task terminal surfaces only)

## Problem

An OUP turn could lose a required tool-call/result, final-assistant, or
review-summary transcript write and still emit a normal `Completed` terminal.
Streaming clients then observed apparent success while a reopen recovered an
incomplete conversation.

## Contract

No RPC method, notification name, event schema, capability, permission,
ledger format, ACK/cursor, or continuation contract changes. When a required
transcript persistence operation fails, the turn preserves the first error
with operation/session/turn context, stops later required writes, and uses
the existing `turn/error` surface with `code=persistence_failed` and partial
result information instead of `Completed`. Review-summary persistence failure
marks its task `Failed`. Successful transcript ordering, tool pairing, cursor
handling, and continuation behavior remain unchanged.

## Validation

Production `add_message_with_seq` boundaries are fault-injected for loop,
final-assistant, and review-summary writes. The pre-fix and controlled
rollback runs reproduce false `Completed` results; the fixed runs emit
persistence failure terminals and preserve reopen/tool-pairing checks. OUP,
turn-loop, focused clippy, formatting, and quality checks are run with the
change.
