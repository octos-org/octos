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

## Follow-up: canonical session title and deletion targets

The same owner resolution also applies to `session/title.set` and
`session/delete`. The fully qualified API session key returned by `session/open`
is used unchanged, including its topic. The legacy short-id candidate expansion
must not prefix it again or proxy it through an unrelated gateway profile.
Authenticated/routed profile checks still apply before store resolution; this
change does not grant API-session mutation access to other channel namespaces.

Cold named-profile runtimes are initialized before locating the session store.
Failure to initialize the authoritative runtime or write/remove persisted data
is returned as an RPC error; the client retains its dialog and can retry. A
missing rename target remains `unknown_session`; deleting an already absent
session remains idempotent. Legacy short-id callers retain candidate lookup.
No method, field, capability, or persistence format changes.

Verification adds canonical-key/topic roundtrips, profile isolation, untouched
prefixed aliases, missing-title targets, and storage-error/retry regressions.
The deployed browser against an isolated real core exercises current, old, and
mobile rename across reload, and deletion of a non-current session across reload.

A successful cwd-scoped `session/list` also remembers each eligible API row's
attested workspace if it has no existing binding. This lets a sidebar mutate an
old project session after a core restart without opening it first. The binding
is limited to the authorized listing profile and never overwrites a live open;
foreign-profile and non-API keys cannot acquire a binding through this path.
