# UPCR-2026-RC81B9296 — Session Permission and Status Ownership

Status: proposed (implemented for review)
Task: T-0625

## Decision record

`permission/profile/set`, `permission/profile/list`, and raw
`session/status/read` are existing-session follow-up operations, not session
creation or discovery operations. Before returning or changing permission
state, the server MUST validate both authorization and existence:

1. On an authenticated WebSocket, reuse the `session/open` canonical profile
   scope check. A non-admin identity may access only a session bound to its
   authenticated profile. A client-supplied `profile_id`, `runtime_mode`, UA, or
   transport claim is not proof of ownership. Cross-scope calls retain the
   existing `-32602` error with `expected_profile_id`, `actual_profile_id`, and
   `auth_scope_violation: true`; these follow-up methods do not add a 1008 close.
2. Empty, unknown, and tombstoned session IDs fail closed with the existing
   `-32100` / `data.kind = "unknown_session"`. Existence means the authoritative
   profile `SessionManager` reports the session through `session_known` and no
   deletion tombstone exists. The authoritative profile runtime never falls
   back to the top-level store after an initialization failure. The existing
   no-LLM/parent-profile legacy cases use the same top-level manager chosen by
   `session/open`. A cold LLM ProfileRuntime is not bootstrapped before the
   read-only root preflight proves the session exists. SET/LIST retain
   the manager guard through the permission operation, closing the DELETE
   tombstone/clear race; raw status releases it before subsequent manager reads.
   Store lock or read failures use
   `runtime_unavailable`, not a fabricated success/unknown.
3. Authorization precedes the dangerous-permission policy check. A valid
   session still receives `permission_profile_disallowed` when its requested
   profile is forbidden; an attacker cannot use that policy error as a substitute
   for ownership enforcement.

Unscoped admin connections retain their explicit cross-profile capability.
Local stdio retains UPCR-2026-016's trusted routing precedence; its last-open
profile is a routing hint, not an authenticated principal. Work-secret typed
SET/LIST retains the exact granted-session gate and additionally requires the
real session record; raw status remains unavailable on session-ingress sockets.
For authenticated WebSockets, a raw SPA ID is owned by the current
authenticated profile; only a structural profile embedded in SessionKey is a
cross-scope claim for these follow-up methods; `session/open` keeps its existing
binding behavior. The bounded process-local open-binding cache is only an
admin/stdio routing hint, never a follow-up ownership or permission-storage key.
Permission state is keyed by `(authorized_profile_id, SessionKey)` at both the
RPC and runtime-permission read paths, so identical raw IDs under two profiles
remain isolated even if an open-binding entry is evicted or rebound. Raw status
also rebuilds context/compaction state from the authorized manager rather than
trusting the unscoped process-global context cache. No method, parameter,
result field, or capability is added.

Cite this record as `UPCR-2026-RC81B9296`.
