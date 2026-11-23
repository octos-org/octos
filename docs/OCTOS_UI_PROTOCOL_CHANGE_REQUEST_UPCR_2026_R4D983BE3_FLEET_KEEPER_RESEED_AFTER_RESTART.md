# UPCR-2026-R4D983BE3 — A scoped fleet keeper runs after a restart with nobody connected

Status: proposed
PR: #3343

## Decision record

No wire shape changes. `UPCR-2026-R4D983BE3` records a change in when the server
runs a fleet keeper's queued wake for a session that was opened with its own
`cwd`.

After a restart, the connection-independent continuation drain runs a wake only
for a session whose workspace it knows. A headless keeper's workspace is
restored from the wake itself: one paired workspace root and cwd scope per
session. That re-seed used to apply to a cwd-scoped session only when no goal
scope was registered for it. Goal scopes are restored from `goal-scopes.json`
at boot (#1973), so after a restart the scope was always registered, the
re-seed was always skipped, and the keeper ran only once a client sent
`session/open` for that session again.

The re-seed now applies when the session has no workspace and its registered
scope is absent or equal to the scope of the wake. A scope is derived from the
session's folder, and the wake's scope and root were recorded together, so an
equal scope means the root is that scope's folder.

Isolation between folders is unchanged:

- A session whose registered scope differs from the wake's is not seeded. Its
  active folder is another one; the keeper is not dispatchable there, and its
  root does not become that folder's workspace.
- A session that already has a workspace is never given a scope.
- `session/open` stays authoritative: it replaces a seeded workspace with its
  own.

No protocol method, notification, field, session or turn identifier, or
capability is added or reinterpreted by `UPCR-2026-R4D983BE3`. A client can observe
one difference: a goal on such a session may make progress after a server
restart before the client reconnects.
