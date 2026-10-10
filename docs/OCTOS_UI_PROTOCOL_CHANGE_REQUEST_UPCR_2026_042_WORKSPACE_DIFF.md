# UPCR-2026-042: read workspace changes without a session preview

- **Request id**: UPCR-2026-042
- **Date**: 2026-10-10
- **Status**: implemented on this branch

## Problem

The clients can read a diff only after receiving a preview ID. A user opening
an existing working tree therefore has no reliable Changes entry before a task
or approval produces a preview. A session preview also does not represent all
uncommitted work, especially when several sessions share a folder.

## Contract

`diff/workspace/get` accepts only `{ "session_id": "..." }`. Unknown fields,
including client-supplied paths, are rejected. The method is advertised alongside
`diff/preview/get` and uses the existing profile, connection and peer admission
checks. It requires an open session with a server-bound workspace.

The response is `DiffPreviewGetResult`: a fresh UUID snapshot, scoped to the
requesting session, with `source: "pending_store"`. The server retains the
snapshot for subsequent `diff/preview/get` reads. It does not emit a preview
notification or make this snapshot the selected task or approval preview.

The snapshot compares the current working tree to HEAD, combining staged and
unstaged changes, and includes nonignored untracked files. An unborn repository
uses the empty tree. Paths are relative to and limited to the bound workspace;
the operation does not write files or alter the Git index. Git external diff,
textconv and fsmonitor hooks are disabled. Binary files retain file entries.

Requests fail explicitly for non-Git workspaces, unsupported filenames, Git
errors, output above 2 MiB, more than 256 changed files, more than 10,000 patch
lines, or execution exceeding 15 seconds. They never return a partial success.

## Clients and compatibility

TUI, web and native clients gate this additive method on advertised support.
They distinguish **Session change preview** from **Workspace uncommitted
changes**. The latter may contain other sessions' or the user's changes and
must not be attributed to the current turn. Existing preview reads remain
available on older servers. Empty, unavailable and failed reads are visible
and closable. Request generation and session checks discard stale replies after
a refresh, scope switch, close or session change.

## Validation

- Real Git repositories: staged, unstaged, untracked, ignored, unborn, empty,
  binary, quoted and spaced paths; workspace boundaries; unchanged index.
- Output and line bounds; a symlink outside the workspace cannot expose content.
- Raw dispatch rejects extra fields, cross-profile access and denied ingress;
  an unopened session cannot select a workspace and stored snapshots remain
  session-scoped.
- Protocol catalog, wire inventory, M18 route inventory and probe parameters
  include the new method. Client tests cover capability gating and stale replies.
