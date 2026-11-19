# UPCR-2026-RB6128701 — Generic turn interrupt reaches the execution owner

Status: proposed
PR: T-0386 (TBD)

## Decision record

`turn/interrupt` keeps its existing wire shape and acknowledgement contract. For
session `spawn_only` background work, the generic interrupt path now resolves
cancellation through `SessionTaskQueryStore`, the same execution-owner bridge
used by explicit `task/cancel`, instead of cancelling only the current turn's
restored `TaskSupervisor` projection.

A restored supervisor remains a read-only projection on this path: it can name
the task to interrupt and refresh the owner's persisted terminal result, but it
is not treated as the producer's cancellation authority. The real owner receives
the cancel token, so its producer terminates through the existing interrupted
branch and cannot later publish a competing `Completed`. If normal completion
wins first, a repeated interrupt remains idempotent and preserves `Completed`.
When no session store exists, the legacy direct-supervisor fallback is unchanged.

The independent `native_subagent` cancellation bridge is unchanged. No protocol
field, session/turn identifier, cross-host routing rule, or capability is added
or reinterpreted by `UPCR-2026-RB6128701`.
