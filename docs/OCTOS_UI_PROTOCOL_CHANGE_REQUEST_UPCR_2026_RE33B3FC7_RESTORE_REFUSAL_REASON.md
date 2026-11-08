# UPCR-2026-RE33B3FC7 — snapshot/restore refusals carry a structured reason

Status: proposed (additive optional error data, backward compatible)
PR: TBD (T-0467)

## Decision record

`UPCR-2026-RE33B3FC7`.

`snapshot/restore` refusals now attach `error.data`:
- `reason: "turn_running"` — a turn is running in the session (restore is
  refused until it is interrupted).
- `reason: "files_edited"` + `paths: [workspace-relative, ...]` — files were
  edited after the snapshot; restoring would overwrite user edits. Paths are
  workspace-relative only (never absolute paths or credentials).

Error code (-32602) and message are unchanged (the message still names the
conflicting files). Other refusals carry no `data` (clients treat a missing or
unrecognized reason as a generic refusal). Additive and optional — older
clients that ignore `error.data` keep working. Backed by the typed
`SnapshotEditConflict` error; default behavior (snapshots enabled gate) is
unchanged.
