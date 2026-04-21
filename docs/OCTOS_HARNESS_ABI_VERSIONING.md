# Octos Harness ABI Versioning

Date: 2026-04-19

Status: stable (v1) for the fields marked stable below.

This document defines the compatibility contract for the harness types that
external app skills, hook consumers, and dashboards depend on. It closes
M4.6 (`#469`) and is the binding reference for future breaking changes.

## Versioned types

The harness exposes four durable serialized types. Every instance carries a
numeric `schema_version` so producers and consumers can negotiate
compatibility:

| Type                          | Crate / module                        | Current version |
| ----------------------------- | ------------------------------------- | --------------- |
| `WorkspacePolicy`             | `octos_agent::workspace_policy`       | 1               |
| `HookPayload`                 | `octos_agent::hooks`                  | 1               |
| `ProgressEventEnvelope`       | `octos_agent::progress`               | 1               |
| `TaskResult`                  | `octos_core::task`                    | 1               |

The current constants are also re-exported at the crate root:

```rust
use octos_agent::{
    HOOK_PAYLOAD_SCHEMA_VERSION,
    PROGRESS_EVENT_SCHEMA_VERSION,
    WORKSPACE_POLICY_SCHEMA_VERSION,
    check_supported,
    UnsupportedSchemaVersionError,
};
use octos_core::TASK_RESULT_SCHEMA_VERSION;
```

Progress events on the wire are identified by both a string schema name,
`octos.harness.event.v1`, and the numeric `schema_version`. The string is
the stable identifier consumers should branch on; the number communicates
minor evolution within that schema family.

## Compatibility rules

1. **Missing `schema_version` deserializes as v1.** Every versioned type
   applies `#[serde(default = "...")]` so pre-M4.6 policy files, hook
   payloads, task results, and progress envelopes continue to load cleanly.
2. **Unknown `schema_version` values greater than the compiled maximum are
   rejected with a typed `UnsupportedSchemaVersionError`, never a panic.**
   The shared helper `octos_agent::check_supported` returns this error; the
   rendered message always includes the type name, the observed version, and
   the supported upper bound so operators can act on it.
3. **Stable fields never change meaning within a major schema version.** The
   tables below enumerate what is stable vs experimental for v1. New
   optional fields MAY be added in the same major version but MUST default
   to an empty / zero value so older consumers ignore them.
4. **Breaking changes bump the major `schema_version`.** A bump requires a
   parallel deprecation window in which both versions load and an entry in
   this document describing the transition.
5. **External consumers should branch on `schema_version` before reading
   version-specific fields.** Rust callers should use `check_supported` to
   enforce the upper bound before trusting the payload.

## Stable vs experimental fields

"Stable" fields are part of the v1 contract; external skills and dashboards
can depend on them. "Experimental" fields may be renamed or removed inside
v1; depend on them at your own risk, and only after branching on
`schema_version`.

### `WorkspacePolicy` v1

Stable:

- `schema_version`
- `workspace.kind` (`slides` | `sites` | `session`)
- `version_control.provider` (`git`), `version_control.auto_init`,
  `version_control.trigger` (`turn_end`), `version_control.fail_on_error`
- `tracking.ignore`
- `validation.on_turn_end`, `validation.on_source_change`,
  `validation.on_completion`
- `artifacts` entries (`primary`, and any role names the app declares)
- `spawn_tasks` map, per-task `artifact`, `artifacts`, `on_verify`,
  `on_deliver`, `on_failure`

Experimental:

- `spawn_tasks.<task>.on_complete` — retained for compatibility with
  pre-M4.6 policies; `on_deliver` is the preferred field for delivery
  actions and will become stable once the last first-party flow migrates.

### `HookPayload` v1

Stable:

- `schema_version`
- `event` (`before_tool_call`, `after_tool_call`, `before_llm_call`,
  `after_llm_call`, `on_resume`, `on_turn_end`, `before_spawn_verify`,
  `on_spawn_verify`, `on_spawn_complete`, `on_spawn_failure`)
- `tool_name`, `tool_id`, `success`, `duration_ms`
- `session_id`, `profile_id`
- `task_id`, `task_label`, `parent_session_key`, `child_session_key`,
  `output_files`, `failure_action` for spawn lifecycle events
- `input_tokens`, `output_tokens`, `cumulative_input_tokens`,
  `cumulative_output_tokens`, `model`, `iteration`, `stop_reason`,
  `has_tool_calls` for `after_llm_call`

Experimental:

- `arguments`, `result` — content is sanitized (sensitive tools redacted,
  non-sensitive truncated to 1 KB). The sanitization contract may grow
  without bumping the schema.
- `session_cost`, `response_cost`, `provider_name`, `latency_ms` — added
  for observability; the numeric basis may be refined within v1.
- `workflow_kind`, `current_phase` — workflow instrumentation; still
  evolving per-app.

### `ProgressEventEnvelope` v1

Stable:

- `schema` (fixed string `octos.harness.event.v1`)
- `schema_version`
- `event.kind` discriminator and the variants: `task_started`, `thinking`,
  `response`, `tool_started`, `tool_completed`, `file_modified`,
  `token_usage`, `task_completed`, `task_interrupted`,
  `max_iterations_reached`, `token_budget_exceeded`,
  `activity_timeout_reached`, `cost_update`
- Variant fields explicitly enumerated in `ProgressEvent` that existed
  before M4.6 (e.g. `TaskStarted.task_id`, `Thinking.iteration`,
  `ToolCompleted.name`, `ToolCompleted.duration`).

Experimental:

- `llm_status`, `stream_chunk`, `stream_done`, `stream_retry`,
  `tool_progress` — currently in flight with M4.1 and may pick up
  additional fields within v1.

### `TaskResult` v1

Stable:

- `schema_version`
- `success`, `output`
- `files_modified`, `files_to_send`
- `subtasks`
- `token_usage.input_tokens`, `token_usage.output_tokens`

Experimental:

- `token_usage.reasoning_tokens`, `token_usage.cache_read_tokens`,
  `token_usage.cache_write_tokens` — optional and omitted when zero; their
  reporting semantics depend on provider support and may tighten inside v1.

## Deprecation and migration rules

1. A field MUST be marked experimental for at least one minor release
   before being promoted to stable.
2. A stable field MUST go through a deprecation window of at least one
   minor release (with `#[deprecated]` on Rust callers and this document
   calling it out) before removal; removal bumps the major schema version.
3. When a new `schema_version` lands, the harness MUST accept the previous
   version until the subsequent major bump so external skills have a
   migration window.
4. Migration guides — one per bumped type — MUST be appended to this
   document as `### <type> v2 migration` sections with a worked example.

## Compatibility tests

`crates/octos-agent/tests/abi_compat.rs` is the binding test suite. It
loads fixtures in `crates/octos-agent/tests/fixtures/` and asserts:

- every versioned type loads from a fixture that includes
  `schema_version = 1`
- every versioned type loads from a pre-M4.6 fixture that omits
  `schema_version` and defaults to v1
- `WorkspacePolicy` is rejected with an actionable error when the file
  claims a future version
- all four first-party policies (`slides`, `sites`, `session`,
  `site-build-output`) round-trip TOML and carry the current version.

Third-party skills SHOULD add similar fixtures for any custom payload they
persist.

## Related documents

- `docs/OCTOS_HARNESS_DEVELOPER_INTERFACE.md` — developer-facing contract
- `docs/OCTOS_HARNESS_M4_WORKSTREAMS_2026-04-21.md` — milestone context
  (M4.6 closes `#469`)
