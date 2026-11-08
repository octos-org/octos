# UPCR-2026-RB06CAB38 — snapshot/restore fails honestly when a created file cannot be removed

Status: proposed (behavior fix, no contract change)
PR: TBD (T-0464)

## Decision record

`UPCR-2026-RB06CAB38`.

`snapshot/restore` previously swallowed `remove_file` failures for files the
undone turn created (e.g. a read-only workspace): it returned success while
the file stayed on disk, so the web client told the user the round was undone
when it was not. The restore now collects such failures and returns an error
naming the un-removable files (same shape as the T-0434 conflict error),
without advancing the last-restore marker so a retry is possible. A partial
restore is reported as partial — never as success. No wire-shape change; the
error message is the contract.
