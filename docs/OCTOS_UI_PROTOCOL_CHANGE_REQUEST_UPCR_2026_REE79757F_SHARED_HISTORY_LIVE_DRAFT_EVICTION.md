# UPCR-2026-REE79757F — Running shared-history drafts are not eviction candidates

Status: proposed (implemented for review)
Task: T-0482 / #2983
Related: UPCR-2026-034, #3033

## Decision record

The global live-draft store retains its soft cap of 64 entries, but an entry
whose exact `(session, turn)` dispatch is still registered in
`RUNNING_TURNS` MUST NOT be evicted. Only terminal drafts are reclaimable.
If every entry is live, the store may temporarily exceed that soft cap,
bounded by the existing `RUNNING_TURNS_MAX` limit.

A pending approval/question and a streamed draft live in different stores;
evicting the latter while retaining the former produced `request → approval`
without the required in-progress row. This change preserves the existing
request → in-progress → status projection order and adds no wire field or
new behavior beyond not losing a live draft. Reused turn ids do not inherit
an earlier dispatch's protection. Locking remains short and non-async; the
running registry is cloned/released before its probe reads the draft store.

Validation: a deterministic 64-running-draft pressure regression fails on
main and passes after the change; the original approval/shared-history
scenario and peer-host-tools suites plus stress runs are recorded in the PR.
Cite this record as `UPCR-2026-REE79757F`.
