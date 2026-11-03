# UPCR-2026-046: server-owned session identity binding for raw wire ids

- **Request id**: UPCR-2026-046
- **Date**: 2026-10-13
- **Status**: proposed
- **Implements**: the minimum contract of #2162 (the admin/unscoped form of #919)

## Problem

`session/open` accepts a raw wire id (no embedded profile — the SPA's
per-tab handle form) plus an explicit `profile_id`, and succeeds. But every
follow-up method resolves the profile from the id itself, the connection's
authenticated scope, or the routed host — none of which carry the open's
explicit `profile_id`. On an unscoped admin connection, a session that just
opened successfully therefore reports `unknown_session` on `session/hydrate`
and routes `session/status/read` to `_main` (`profile_unresolved` when
`_main` is not configured), while the identical sequence with the canonical
profile-qualified id works. `session/open` returns a half-usable identity.

## Contract

1. Every successful `session/open` records the profile it resolved under,
   keyed by the wire id the reply echoes (what the client will send back) —
   but only when the binding is load-bearing: a canonical id resolves
   through its own embedded profile, and a profile-less open shares the
   `_main` / process-wide manager with every lookup already. Reopening the
   same wire id under another profile rebinds it — the last open owns the
   wire id.
2. Session identity consumers consult the binding as the WEAKEST tier,
   after the id's own profile, the connection's auth scope, and the routed
   host: the shared sessions-manager lookup (`resolve_sessions_for_lookup`
   — hydrate, rollback, fork, thread graph, turn state, btw, context
   status), the turn/aside profile resolution (turn start and `session/btw`
   — whose per-connection last-open cell fallback this replaces for
   addressing; a per-session binding cannot misroute one socket's second
   session), `session/status/read`, `session/compact`, and voice admission.
   `session/fork` binds a forked child of a raw parent to the profile the
   parent resolved under, so the fork does not mint another unusable
   identity. Explicit scopes are never overridden, so profile-scoped and
   host-routed connections keep their existing isolation.
3. The binding is per-process state. After a serve restart a raw-id lookup
   degrades to today's behavior until the session is reopened.

## Residual (deliberately not covered here)

- A raw id opened under a profile whose runtime cannot materialize (e.g. no
  LLM selection) lands in the process-wide manager with no binding; a later
  runtime materialization for that profile does not retroactively re-route
  the id. Fix-before and fix-after behave identically; closing this needs
  durable identity, below.
- Restart-persistent identity: a durable binding needs the catalog's
  storage (#2146), not another sidecar map. The durable, server-owned
  SessionRef/catalog contract remains the stronger fix.

## Non-goals

- The durable, server-owned SessionRef/catalog contract (#2146) — this
  record is the in-process minimum that stops returning half-usable
  identities; the catalog remains the stronger fix.
- `permission/profile/list`'s profile-less success shape (unchanged).
- The continuation drain filter's per-connection profile hint (a delivery
  filter, not identity addressing) — unchanged.
