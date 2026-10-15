# UPCR-2026-049: oversized-frame preview runs off the executor worker

- **Request id**: UPCR-2026-049
- **Date**: 2026-10-14
- **Status**: implemented
- **Implements**: the offload half of #2446 (the memoize half is a
  non-goal below)

## Problem

Every outbound frame flows `frame_for` → `frame_text_within_cap` →
`preview_oversized_frame`, so an over-cap frame — a multi-MB
`session/hydrate` result, a giant tool output — pays the truncation's full
CPU cost (parse + measure + full-tree walk + up to two full
serializations, with the structural fallback re-serializing per round)
inline on the connection task's executor worker. Measured on a ~15 MiB
frame: a ~1.1 s CPU stretch during which every other task scheduled on that
worker is frozen (a 1 ms ticker advanced 0 times; with the offload it
advances ~370 times over the same stretch). The frame bytes are identical
before and after; only scheduling changes.

## Contract

1. **Byte-identical output.** The truncation algorithm is unchanged — the
   function splits into a cap-check + dispatch wrapper and the original
   body verbatim. The same input produces the same bytes on every path.
2. **Where it runs.** On a multi-threaded runtime worker (the connection
   task's shape) the stretch goes through `tokio::task::block_in_place`,
   which parks the worker's scheduler so co-scheduled tasks migrate to the
   replacement worker while the truncation runs. The flavor check keeps the
   one panic case out — `block_in_place` panics on a current-thread flavor —
   so those runtimes (and threads with no runtime at all, i.e. sync tests)
   take the inline path. On a multi-thread `block_on` calling thread or a
   `spawn_blocking` pool thread the guard still selects `block_in_place`,
   and tokio runs it inline there — legal, just without the parking. No
   LocalSet and no `spawn_blocking` caller reaches the preview today, so
   every caller today gets exactly the contract above; any future one gets
   inline semantics, never worse than before this record.
3. **No schema change.** No new method, notification, payload field, enum
   variant, capability bit, or wire value of any kind. The frame a client
   receives is byte-for-byte what it received before.

## Why this record exists

The implementation edits
`crates/octos-cli/src/api/ui_protocol_transport.rs`, and the completeness
gate (`scripts/check-ui-protocol-upcr.sh`) treats any edit to a
`ui_protocol_*.rs` file as protocol-visible. The change adds no protocol
surface, but it changes a client-observable latency posture (co-scheduled
requests no longer stall behind a hydrate's truncation), so the scheduling
contract is recorded rather than left implicit in a code comment.

## Non-goals

- **Memoizing the truncated form** (the second half of #2446): caching
  per-frame multi-hundred-KB previews needs a memory-budget policy (entry
  count/byte cap, eviction, keying) that deserves its own record; until
  then every hydrate recomputes — just without starving the worker.
- **`spawn_blocking` instead of `block_in_place`**: it would move the work
  to the blocking pool but forces the sync `frame_for` chain (and every
  sync sender above it) through an async boundary — a far larger surface
  than the starvation fix needs. `block_in_place` keeps the call chain
  untouched.
- **Bounding the preview cost itself**: the truncation is single-pass by
  design; making it cheaper is orthogonal to where it runs.
