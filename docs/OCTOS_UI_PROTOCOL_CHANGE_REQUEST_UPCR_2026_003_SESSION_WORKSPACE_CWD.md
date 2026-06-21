# Octos UI Protocol Change Request: Session Workspace CWD

## Header

- Request id: `UPCR-2026-003`
- Title: Add per-session workspace `cwd` binding to `session/open`
- Author: M9 protocol workstream
- Date: 2026-04-24
- Target protocol: `octos-ui/v1alpha1`
- Status: accepted
- Related issue: M9 protocol contract

## Summary

This change request lets an AppUI client request a workspace directory when
opening a session. The server canonicalizes and validates the requested `cwd`,
binds the session runtime to the approved workspace, and echoes the approved
root as `workspace_root`.

The change is additive and capability-gated. It fixes the class of bugs where
an AppUI session visually points at one project while shell/file tools execute
against the daemon or profile default directory.

## Wire Contract

Feature flag:

- `session.workspace_cwd.v1`

Method:

- `session/open`

Optional params:

- `cwd`: requested workspace directory

Optional result fields:

- `workspace_root`: canonical server-approved workspace root

Rules:

- Clients may send `cwd` only when `session.workspace_cwd.v1` was negotiated.
- Servers must expand and canonicalize `cwd`, require it to exist as a
  directory, validate it against profile/workspace policy, and reject unsafe
  system roots.
- A running session's first accepted workspace is sticky until that session
  runtime is evicted. A later `session/open` for the same session with a
  different `cwd` must not silently rebind tools; the response should continue
  to report the cached `workspace_root`.
- Profile-scoped sessions require a configured profile runtime. If the server
  cannot bind the workspace, it must return a typed error such as
  `cwd_runtime_unavailable`, `cwd_not_accessible`, `cwd_not_directory`, or
  `cwd_system_path_banned`.
- When no client `cwd` is supplied, servers may use an operator configured
  default workspace and still report it through `workspace_root`.

Example:

```json
{
  "jsonrpc": "2.0",
  "id": "open-1",
  "method": "session/open",
  "params": {
    "session_id": "ada:local:tui#coding",
    "profile_id": "ada",
    "cwd": "/Users/ada/project"
  }
}
```

## Compatibility

- `cwd` and `workspace_root` are additive.
- Servers that did not negotiate `session.workspace_cwd.v1` must reject a
  non-empty `cwd` instead of ignoring it.
- Old clients that omit `cwd` continue to open sessions through the prior
  workspace fallback path.
- This UPCR does not add in-session cwd mutation. Future mutation UX or
  persistent cwd approval policy requires a separate accepted UPCR.

## Tests

Coverage lives in:

- `crates/octos-core/src/ui_protocol.rs`
  - `SessionOpenParams.cwd` round-trips and remains optional for legacy
    payloads
  - `workspace_root` round-trips in `SessionOpenResult`
- `crates/octos-cli/src/api/ui_protocol.rs`
  - `session/open` rejects `cwd` without the negotiated feature
  - client-supplied `cwd` drives the session runtime workspace
  - two sessions on one profile with different cwds stay isolated
  - reopening the same session with a different cwd reports the cached root
  - unregistered profiles and unsafe/system paths return typed errors
  - operator default `appui.default_session_cwd` works when no client cwd is
    supplied

## Decision

Accepted by the M9 protocol workstream. The workspace cwd surface was already
implemented and linked from the spec; this document restores the missing
change-control record.
