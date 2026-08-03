# UPCR-2026-027: Skill Action Background Jobs

Status: accepted
Date: 2026-07-09
PR: TBD

## Summary

Add a generic AppUI background-job surface for manifest-declared skill actions:
`skill/action/job/list`, `skill/action/job/read`, and the
`skill/action/job/updated` notification, advertised through
`skill.action_jobs.v1`.

This extends UPCR-2026-026 without making NotebookLM or source import a backend
protocol primitive. Skills decide which actions are background-capable through
their manifest. AppUI clients can start an action and observe job progress, but
they still cannot invoke arbitrary tools or override a skill-owned binding.

## Decision

Do add an optional `execution` field to skill manifest actions:

- `sync` (default): existing UPCR-2026-026 behavior
- `background`: `skill/action/invoke` enqueues one or more durable jobs and
  returns immediately

Do model each background invocation as a supervised background task. A
`file_each` action creates one task per materialized file and groups their job
projections with a shared `batch_id`. `job_id` equals the canonical `task_id`;
the job API does not own an independent lifecycle.

Do support these statuses:

- `queued`
- `running`
- `succeeded`
- `failed`
- `cancelled`
- `abandoned`

Do persist tasks through the standard per-session task ledger so clients can
reconnect and query status. Queued or running tasks are not auto-resumed after
process restart; the supervisor's orphan sweep projects them as `abandoned`.

Do derive `skill/action/job/updated` from the same task transition through a
named `TaskSupervisor` listener. Named listeners fan out without replacing the
runtime's primary `set_on_change` consumer.

Do enforce the job snapshot's `profile_id` at both replay and live fan-out.
A connection bound to another profile must not receive the notification even
when both profiles use the same bare wire `session_id`.

Do NOT add `/api/notebook/*` routes or a notebook-specific job type. Notebook
source import is one consumer of generic skill action jobs.

Do NOT let clients create arbitrary background tool calls. The manifest action
still owns the tool binding, input mode, file argument, file materialization,
execution mode, and UI hints.

## Capabilities

Feature token:

- `skill.action_jobs.v1`

Methods:

- `skill/action/job/list`
- `skill/action/job/read`

Notification:

- `skill/action/job/updated`

The methods are server-handled `APPUI_EXTRA_METHODS` and require the same
profile-backed session runtime as skill actions.

## Manifest Field

Action manifests may include:

```json
{
  "execution": "background"
}
```

If omitted, execution defaults to `sync`.

## AppUI Surface

### `skill/action/invoke`

For `execution: "background"` actions, the request shape remains unchanged:

- `session_id` — required
- `profile_id` — optional profile override
- `action_id` — action id or `skill_id/action_id`
- `arguments` — optional JSON object

The response is:

- `action_id`
- `ok`
- `batch_id`
- `jobs[]`

Each job entry includes at least `job_id`, `batch_id`, `session_id`,
`profile_id`, `action_id`, `skill_id`, `status`, `created_at`, and
`updated_at`. File-based jobs also include `input_path`, `filename`, and
`materialized_path` when available.

A completed job's generic `result.artifacts[]` entries contain `handle`,
`display_name`, `media_type`, and `size`. The `ws/...` handle is opaque and
must be resolved with the owning `session_id`; raw host paths are never exposed.
Files outside the session workspace, missing files, and traversal paths are
omitted. `result.file_modified`, when present, is also an opaque handle.

### `skill/action/job/list`

Request:

- `session_id` — required
- `profile_id` — optional profile override
- `batch_id` — optional filter
- `action_id` — optional filter

Response:

- `profile_id`
- `session_id`
- `count`
- `jobs[]`

The response returns the latest snapshot per job, sorted by creation/update
time in a stable server-defined order.

### `skill/action/job/read`

Request:

- `session_id` — required
- `profile_id` — optional profile override
- `job_id` — required

Response:

- `job` — latest snapshot for the requested job

Missing jobs return a typed AppUI invalid-params/not-found style error rather
than an empty success payload.

### `skill/action/job/updated`

Notification payload:

- `profile_id`
- `session_id`
- `job`

The `job` object is the same latest-snapshot wire shape used by list/read.

## Job Record

The canonical wire fields are:

- `job_id`
- `batch_id`
- `profile_id`
- `session_id`
- `action_id`
- `skill_id`
- `status`
- `input_path`
- `filename`
- `materialized_path`
- `output`
- `error`
- `result`
- `created_at`
- `updated_at`

`result` is a generic skill-owned result envelope. Source-oriented skills may
put structured metadata within it, but the AppUI job protocol does not project
notebook-specific fields.

## Persistence

The canonical task ledger is append-only per session:

```text
<session-store>/users/<encoded-base>/sessions/<encoded-topic>.tasks.jsonl
```

Skill fields (`batch_id`, action/skill identity, input paths, and result) live
in opaque task projection metadata. Status, timestamps, output, errors,
cancellation, and restart recovery come only from `TaskSupervisor`. Its orphan
sweep marks stale active tasks failed with the restart-orphan reason; the job
adapter maps that specific transition to `abandoned`. Explicit supervisor
cancellation maps to `cancelled` and is never rewritten as failure.
Because `job_id` equals `task_id`, the existing supervised task cancellation
control is the only cancellation path; there is no second skill-job cancel
state machine.

## Compatibility

Backward-compatible. Existing manifests omit `execution` and keep synchronous
UPCR-2026-026 behavior. Existing clients that only know `skill.actions.v1` see
and can call synchronous actions only. Background actions are omitted from
their action list and direct invocation fails with `method_not_supported`.
Clients that support background actions must negotiate `skill.action_jobs.v1`
before relying on job methods or notifications.

## Tests

- Manifest parsing covers `execution: "background"` and default `sync`.
- Task projection tests cover listing, reading, cancellation, and restart
  recovery from the canonical task ledger.
- AppUI route tests cover missing params, missing jobs, capability
  advertisement, and method dispatch.
- Background invoke tests cover one job per `file_each` input and job snapshots
  progressing through queued/running/succeeded or failed.
- The reproducible registry is
  `e2e/fixtures/compat-test-skill/manifest.json`
  (`compat-test-skill@1.0.0`), exporting concrete `source.import` and
  background `reports.generate` actions. Production action packages and their
  pinned registry commit are listed in UPCR-2026-026.

## References

- UPCR-2026-026 for manifest-declared skill action discovery and sync invoke.
- `crates/octos-cli/src/api/ui_protocol.rs::APPUI_EXTRA_METHODS`.
- `crates/octos-core/src/ui_protocol.rs::UI_PROTOCOL_NOTIFICATION_METHODS`.
