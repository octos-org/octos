# UPCR-2026-064 — Session-bound profile resolution for workspace files

- Status: proposed (implemented for review)
- PR: #2915

## Problem

`session/files/read` resolved the session workspace from the requester's default
profile. An unscoped admin token therefore looked under `_main` even when
`session/open` had returned a named `active_profile_id` such as `coding`, and the
read failed with `session_workspace_not_found`. The same requester-default lookup
also affected session-relative `/api/files` downloads and mutations.

## Change

1. Resolve file access from the profile that actually owns the opened session:
   the embedded `session_id` profile first, then the server-recorded
   `session/open` binding for raw `web-*` ids. Use that profile's data dir for
   the legacy workspace fallback as well as the live workspace binding.
2. Apply this scope to `session/files/read`, session-relative `/api/files` and
   delivered copies, and file mutation. Upload ownership and other
   requester-scoped file rules remain unchanged.
3. WS `validate_session_scope` also honors the recorded open binding, so
   `diff/preview/get` and other session RPCs cannot bypass profile scope with a
   raw id belonging to another user.

## Authorization

- An unscoped admin connection retains the existing model and is authorized for
  every profile, including named-profile sessions.
- A user connection may read only a session whose bound profile matches its
  authenticated/routed profile. A mismatch returns
  `workspace_file_access_denied` on REST file reads and `auth_scope_violation`
  on WS session methods.
- Legacy unauthenticated local requests remain confined to `_main`.
- Explicit profile rebind of a raw session id by unscoped admin remains allowed;
  the recorded binding changes only after a successful open. No new cross-user
  rebind is permitted.

## Compatibility and limits

No new RPC method, feature token, field, or persistence format is introduced.
Existing workspace path confinement, no-follow reads, size caps, download
ownership, and typed error shapes are unchanged. Evicted sessions without an
embedded or recorded profile keep the previous requester-profile fallback;
process restart therefore does not broaden access.

## Verification

- Named-profile session read by unscoped admin and the owning user; another
  profile is denied.
- Raw-id WS binding accepts admin/owner, rejects another profile, and preserves
  explicit unscoped-admin rebind.
- Existing handlers, session-scope, and diff-preview suites.
- Real first-run octoscode-web wizard creates `coding`; an agent writes
  `notes.md`, and artifact Preview displays its contents without
  `session_workspace_not_found`.
