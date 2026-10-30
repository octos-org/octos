# UPCR-2026-060 — Durable Workspace Catalog and SessionRef

Status: proposed (implemented for review)
Task: T-0041 / issue #2146

## Problem

A fresh authenticated client cannot discover prior work without already knowing a host cwd and a bare session id. `SessionInfo` lacks the Workspace/profile/storage identity, and `session/list` is not a server-owned catalog.

## Contract

- `Workspace` has a server-generated opaque `workspace_id`, canonical path, display name, allowed profile scopes, timestamps, availability, writability, and archived state.
- `SessionRef` is a server-minted opaque ref. It maps to one `(workspace_id, profile_id, session_key, storage_kind, store_root)` binding; clients never parse or synthesize it. The same legacy id in different Workspace/profile stores has distinct refs.
- `workspace/list|register|get` expose only the authenticated profile's Workspace catalog.
- `session/create {workspace_id,profile_id?}` creates and returns a ref; `session/open {session_ref}` resumes only and never creates an unknown session. `created` is reported on the open result.
- `session/list {workspace_id?,cursor?,limit?}` returns a paginated catalog with `SessionRefRecord`, Workspace summary, profile, title, timestamps, storage kind, and active-turn state. Unscoped listings include authorized cwd-scoped and profile-global records; legacy global sessions without reliable cwd stay `workspace_id = null`.
- Identity-sensitive methods/events will converge on SessionRef in a follow-up; this PR does not add more optional cwd parameters.

## Allowed roots

- Default registration is limited to the actual `serve --cwd` plus explicit `OCTOS_WORKSPACE_ALLOWED_ROOTS` entries. The same rule applies to host-managed operation; no implicit home-directory or `/` grant exists.
- Registration canonicalizes both the requested path and the approved roots and rejects paths outside them. Archived/unavailable roots cannot create sessions.

## Migration and compatibility

- Workspaces are imported only from the existing `session-workspaces` metadata. Session rows are indexed from known session metadata and already-used cwd stores; there is **no filesystem crawl** for `.octos` directories and no transcript migration or rewrite.
- Legacy clients that do not negotiate `workspace.session_refs.v1` keep the old `session/open` and `session/list` request/result paths unchanged. SessionRef result fields are additive and only emitted on the negotiated path.
- External host-managed connections cannot enumerate the catalog, register Workspaces, or create sessions; they may only resume the SessionRef already bound to their connection. Other refs are refused.

## Persistence

`workspaces.json` and `session-catalog.json` live beside the profile's existing metadata. Transcripts remain in their existing `<cwd>/.octos` or profile-global stores.

## Validation

- Registry tests: same legacy id in two Workspaces gets distinct refs, unknown refs fail, outside roots are refused.
- WS/stdio compatibility and solo probes cover Workspace list/register/get, create/list/open, unresolved legacy rows, and unknown-ref behavior.
