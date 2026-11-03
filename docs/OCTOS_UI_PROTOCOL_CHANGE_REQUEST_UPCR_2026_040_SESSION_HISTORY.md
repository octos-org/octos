# Octos UI Protocol Change Request: Persistent Project Session History

## Header

- Request id: `UPCR-2026-040`
- Date: 2026-10-06
- Target protocol: `octos-ui/v1alpha1`
- Status: implemented
- Scope: additive raw AppUI method `session/history/list`

## Problem

OctosCode's session switcher cannot discover project-local terminal sessions
using the workspace-scoped `session/list`. In-memory workspace addresses also
disappear on server restart, leaving existing projects absent from the catalog.

## Contract

`session/history/list` accepts optional `workspaces`, `profile_id`, `offset`
and `limit`. It returns `sessions`, `total`, `next_offset`, `workspaces`,
`unavailable_workspaces` and `coverage: "profile_and_known_workspaces"`.
The limit defaults to 100 and is clamped to 1..200; at most 128 explicit
workspace hints are accepted. Each row adds `profile_id` and `workspace_root`
to the existing session metadata, with a fully qualified `id`. Clients retain
the compound identity `(profile_id, workspace_root, id)` when opening a row.

The server advertises this additive method. Older clients continue using
`session/list`; newer clients may fall back to workspace-only listing when
the method is absent. The request and result use the existing raw AppUI JSON
extension path rather than adding a core `UiCommand` variant.

Ordinary project session startup remembers the canonical project address in
the owning profile's `session-workspaces/` directory. Validated explicit hints
with existing session stores are remembered too. Atomic individual address
records support concurrent local terminal/server processes. Discovery combines
these records with explicit hints and current ordinary workspace bindings.
It does not recursively scan the user's filesystem or move transcripts.

## Authorization and failure behavior

Frozen profile scope applies to every row and saved address. Cross-profile
requests are rejected; only unscoped/admin connections can enumerate profiles.
Session-ingress credentials cannot call or advertise the method, and the
host-managed external allowlist remains unchanged. Child transcripts and
host-bound or refused peer/context transcripts are excluded.

Saved addresses grant no permissions. Every project passes the same current
workspace gate as `session/list` before its store is opened. Missing or denied
folders are reported as unavailable, without recreating them. Corrupt address
records return an error instead of silently reporting complete coverage.
With project session storage disabled, only profile stores are enumerated.

## Validation

- `session_history_should_isolate_profiles_and_paginate`
- `session_history_should_find_saved_workspaces_without_a_client_hint`
- `session_history_should_remember_existing_hints_but_not_create_empty_stores`
- `should_preserve_workspace_addresses_without_process_state`
- `should_keep_simultaneous_registrations_of_different_projects`
- `bootstrap_relocates_store_to_cwd_when_flag_on`
- Existing capability, stdio dispatch and protocol catalog parity tests
