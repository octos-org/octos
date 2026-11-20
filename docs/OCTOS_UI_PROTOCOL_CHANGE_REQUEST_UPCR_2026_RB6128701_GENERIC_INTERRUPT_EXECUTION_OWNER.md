# UPCR-2026-RB6128701 — Generic turn interrupt reaches the execution owner

Status: proposed
PR: T-0386 (TBD)

## Decision record

`turn/interrupt` keeps its existing wire shape and acknowledgement contract. For
session `spawn_only` background work, the generic interrupt path now resolves
cancellation through `SessionTaskQueryStore`, the same execution-owner bridge
used by explicit `task/cancel`, instead of cancelling only the current turn's
restored `TaskSupervisor` projection.

A restored supervisor is not used as a substitute for a reachable execution
owner: it can name the task to interrupt and refresh the owner's persisted
terminal result, while the real owner receives the cancel token. Its producer
therefore terminates through the existing interrupted branch and cannot later
publish a competing `Completed`. If normal completion wins first, a repeated
interrupt remains idempotent and preserves `Completed`. When the owner is gone
or out of process, the store retains its existing `task/cancel` fallback and may
cancel the restored projection; this change does not invent a cross-process
owner. Store-less `octos serve` non-stdio callers likewise retain the legacy
direct-supervisor fallback.

The independent `native_subagent` cancellation bridge is unchanged. No protocol
field, session/turn identifier, cross-host routing rule, or capability is added
or reinterpreted by `UPCR-2026-RB6128701`.
