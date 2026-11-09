# UPCR-2026-R4B3555BE — Peer checkpoints and bounded gather waits

Status: proposed (implemented for review)
PR: https://github.com/octos-org/octos/pull/3142

## Decision record

The serve/WS turn path exposes `peer_publish` to staged peer sessions so a
peer can publish an intermediate artifact checkpoint before its turn ends.
The host derives the peer identity from the active session, verifies that it
is the trusted session for the caller's profile, and rejects closed peers and
empty or oversized messages. The latest checkpoint is atomically replaced in
the peer's local blackboard; it is not a completion, a review, an instruction
to another peer, or a substitute for the final result.

`peer_gather` can include that checkpoint while the peer remains open. It
labels the recorded turn as current, previous, or unverified and ignores a
checkpoint whose session is no longer trusted. Its optional `wait_ms` waits up
to 60 seconds for the selected blackboard view to change. Probes do not mark
results consumed; only the final, untruncated gather can record eligible
result receipts. A timeout does not imply completion. Hosts without a wait
probe reject nonzero waits, and existing calls without `wait_ms` retain their
immediate behavior.

This changes model-visible tool behavior and local peer checkpoint state. It
does not add a UI Protocol wire method, DTO, capability, or lifecycle event.
Existing clients and blackboards without a checkpoint remain supported. The
checkpoint is scoped to the staged peer and active session, and no guarantee
is made that it survives peer cleanup or session replacement.
