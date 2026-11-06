# UPCR-2026-RDA916EB5 — `session/files.list`: workspace tree listing with bounded pagination

Status: proposed (implemented for review)
PR: TBD

## Decision record

`UPCR-2026-RDA916EB5`.

`session/files.list` historically scanned only the legacy
`users/<base>/workspace` layout, so cwd-bound sessions (web chat with an
explicit `cwd`) always got an empty list even with a non-empty workspace —
`files/read` already resolved the authoritative workspace root, the listing
did not. The handler now resolves the session's workspace the same way
`files/read` does (the session-bound root first, legacy dirs as fallback)
and lists that tree.

Additive wire changes:

- `SessionFilesListParams` gains optional `prefix`, `depth`, `limit`,
  `cursor`. A request WITHOUT any of them keeps the legacy contract — a
  flat, files-only listing (directories are never emitted), now resolved
  from the authoritative workspace root instead of only the legacy
  `users/<base>/workspace` layout — bounded by the hard ceilings below.
  Old clients see the same shape and entry semantics they always saw;
  cwd-bound sessions simply stop returning an empty list.
- Tree entries are JSON objects `{filename, path, kind, size_bytes,
  modified_at}` where `path` is the workspace-relative, forward-slash
  path accepted by `session/files/read`, and `kind` is `"file"` or
  `"dir"`. Entries are sorted by `path` (deterministic; the legacy
  modified-at order only applies to param-less requests).
- `SessionFilesListResult` gains optional `next_cursor` (present iff more
  entries follow; feed back as `cursor`). Pagination is keyset on `path`.
- Bounds: `depth` defaults to 1 (immediate children — the lazy tree shape),
  capped at 8; `limit` defaults to 500, capped at 2000; a call walks at most
  a fixed number of entries per page.
- Exclusions: `.git`, `.svn`, `.hg`, and the internal `.octos` session
  store are never listed. Symlinks are never traversed as directories; a
  symlink whose canonical target escapes the workspace is skipped.
  `prefix` must be workspace-relative (no absolute, no `..`, no `\`);
  canonicalized it must stay inside the workspace root. Entries with
  non-UTF-8 names are skipped (the string protocol cannot address them
  without identity ambiguity).
- Authorization is unchanged: routed-profile layer-2 gate plus the
  session-owner check mirrored from `files/read`; a session without a
  resolved workspace root falls back to the legacy scan.
