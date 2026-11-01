# UPCR-2026-R57D60D1E — Transport Fixture Isolation

Status: proposed (implemented for review)
PR: #3001
Task: T-0175

## Decision record

UPCR-2026-R57D60D1E records an internal build-boundary cleanup with no UI
Protocol contract change. M9/M14/M15 deterministic fixtures, magic-prompt
routing, their dedicated spawn helper, forced-backpressure support, and soak
evidence writers compile only for tests or the explicitly non-default
`transport-test-fixtures` feature.

No RPC method, parameter, result, notification, capability name, ledger event,
cursor, SessionActor behavior, or durable-delivery ordering changes. Production
continues to construct and persist the same normal OUP events; only the
fixture/evidence side effects are absent from a default production binary. The
fixture feature enables only the existing `api` feature and does not change the
normal dependency tree. Explicit nightly/manual fixture lanes retain the same
scenarios and diagnostics.
