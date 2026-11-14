# UPCR-2026-R2E4C10A7 — Durable explicit Session permissions

- Status: Proposed, stacked on UPCR-2026-RC81B9296 (T-0625 ownership).
- Task: T-0626 / T-0621 F02.
- Methods: existing `permission/profile/set`, `permission/profile/list`, and `session/status/read`.
- Wire compatibility: no request or result field is added or removed. Durable storage and failure semantics change.

## Contract

1. Every successful SET persists the complete effective selection (`mode`, `network`, and explicit `approval_policy`/`inherit`) before updating process memory or returning success. This includes a selection equal to the current server default.
2. The versioned record is rooted at the authorized `ProfileRuntime.data_dir` (or the legacy top-level manager data dir), not a project-local sessions root. Its path and payload both bind `(profile_id, SessionKey)`. Project-local transcript routing may vary; permission identity does not.
3. Only a missing record is legacy and inherits the configured server default. Invalid JSON, missing/unknown fields, unsupported versions, invalid enum values, identity/path mismatch, and read/write I/O failures are not legacy.
4. Invalid records return `session_permission_invalid`; I/O failures return `session_permission_io` with a structured RPC error. Neither may fall back to wider defaults. Failed SET never returns an applied result and invalidates the cached runtime.
5. Permission records are lazily loaded before every SessionRuntime bootstrap and before LIST/SET/raw status reads. Background continuation uses the same effective-permission path. Successful SET retains the existing epoch invalidation; this does not retroactively cancel an already-running tool call.
6. SET anchors the authoritative durable session record so a metadata-only session remains readable after restart. DELETE removes the permission sidecar with the other session settings.
7. Persisted dangerous choices are revalidated against the current runtime/deployment mode during recovery; a record cannot bypass the current policy gate.

## Acceptance

The regression starts and stops the real `octos` binary twice against the same private data directory, in solo and non-solo modes, with no model calls. It covers read-only+network-deny+approval restoration, legacy defaults, write failure and poison, and corrupt/partial records. A storage unit covers delete/recreate non-resurrection. T-0625 ownership and work-secret tests remain mandatory.
