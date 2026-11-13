# UPCR-2026-R8F2AFCF0 — session/history/list degraded-flat diagnostic

Status: proposed
PR: T-0216

## Decision record

A project workspace whose flat `sessions` branch exists but is not a usable
directory (plain file, dangling/loop link) previously either redirected users
discovery to a link target's parent or swallowed the store's legal `users`
sibling. The fix (a) canonicalizes the flat branch only when it is a real
directory, and (b) reports such a workspace in `unavailable_workspaces` while
keeping the legal `users` sibling rows. The diagnostic uses `is_dir()`
(follows links), so a symlink TO a real sessions directory stays valid,
matching the bus walk. Additive to `unavailable_workspaces`; no existing field
changes. Read-only enumeration never creates the missing directory. Cite this
record elsewhere as `UPCR-2026-R8F2AFCF0`.
