# UPCR-2026-R9631FCFB — Authenticated Bounded Catalog Bootstrap

Status: proposed (implemented for review)
PR: TBD
Task: T-0404 / issue #2146

## Decision record

UPCR-2026-R9631FCFB adds the explicitly negotiated `catalog/bootstrap` RPC
and `catalog.bootstrap.v1` capability for a newly authenticated client. Auth
responses remain unchanged and never embed the catalog. The client explicitly
requests a page after authentication with optional `workspace_id`, `profile_id`,
`cursor`, and `limit` parameters.

The result contains the server-authorized `connection_scope.profile_ids`,
authorized Workspace summaries on the first response (only page-relevant
summaries on continuations), `SessionRefRecord` rows, an opaque
`next_cursor`, and a summary with total filtered Workspace/Session counts,
returned-row count, and `has_more`. `limit` defaults to 50 and is clamped to
1..=100. Pagination reuses the existing `(updated_at_ms DESC, session_ref ASC)`
keyset, including when the page spans multiple authorized profiles. Existing
refs and Workspace bindings remain stable across process and browser restarts.

The implementation reconciles only the existing `workspaces.json` and
`session-catalog.json` registries plus the same known session metadata used by
`session/list`. It neither creates a second registry nor crawls the host for
unknown `.octos` directories. An unscoped admin receives the authorized
all-profile catalog rather than an implicit `_main` subset. An authenticated
user receives only that user's scope; a host-routed or connection-scoped
connection cannot widen itself by supplying another `profile_id`. Workspace
summary `profile_ids` are reduced to the authorized scope before serialization.

External host-managed connections are denied catalog enumeration. Their bound
SessionRef resume rule is unchanged. Legacy clients that do not negotiate
`catalog.bootstrap.v1` keep the existing `session/list` behavior and receive
`method_not_supported` for the new method. The feature is not a stdio default;
a stdio client may explicitly negotiate it through `client_hello`.
