# UPCR-2026-R0C02A641 — Ingress Background Scope

Status: proposed (implemented for review)

## Decision record

An ingress socket now binds its background-work generation to the profile that
turn admission would resolve: the session key's profile, then the connection
profile, then the profile recorded by `session/open`, then the profile runtime.
Previously it used the connection profile or `_main`, so revocation could miss
work running under a profile recorded by `session/open`. The scope is stored at
bind and reused at revocation. Matching a running turn still uses the existing
`ActiveTurn.profile_id` value.

No method, DTO, capability, lifecycle state, or persistence schema changes.
Revocation remains scoped per profile. Connections whose resolved profile
matches the previous value behave as before.

Known limitation: the scope is fixed at bind. If `session/open` first records
the profile on the same socket after the bind, the generation stays under
`_main` and revocation does not reach that work.
