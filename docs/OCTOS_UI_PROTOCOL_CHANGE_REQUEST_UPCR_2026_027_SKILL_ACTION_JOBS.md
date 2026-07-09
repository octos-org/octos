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

Do model each background invocation as server-owned job snapshots. A `file_each`
background action creates one job per materialized file and groups those jobs
with a shared `batch_id`.

Do support these statuses:

- `queued`
- `running`
- `succeeded`
- `failed`
- `abandoned`

Do persist the latest job snapshots so clients can reconnect and query status.
Queued or running jobs are not auto-resumed after process restart; startup
recovery marks them `abandoned`.

Do emit `skill/action/job/updated` after each appended job snapshot.

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
- `source_id`
- `source_path`
- `metadata_path`
- `created_at`
- `updated_at`

Notebook source metadata fields are optional convenience projections extracted
from a skill result's structured metadata. They do not make the job protocol
notebook-specific; non-source skills can ignore them and use `result`.

## Persistence

Job snapshots are append-only per session:

```text
<profile_data_dir>/skill-action-jobs/<encoded-session-id>.jsonl
```

The latest snapshot for a `job_id` wins. Startup recovery scans persisted job
files and appends `abandoned` snapshots for jobs whose latest status is
`queued` or `running`.

## Compatibility

Backward-compatible. Existing manifests omit `execution` and keep synchronous
UPCR-2026-026 behavior. Existing clients that only know `skill.actions.v1` can
continue to call synchronous actions. Clients that support background actions
must negotiate `skill.action_jobs.v1` before relying on job methods or
notifications.

## Tests

- Manifest parsing covers `execution: "background"` and default `sync`.
- Job store tests cover latest-snapshot listing, reading, and restart recovery.
- AppUI route tests cover missing params, missing jobs, capability
  advertisement, and method dispatch.
- Background invoke tests cover one job per `file_each` input and job snapshots
  progressing through queued/running/succeeded or failed.

## References

- UPCR-2026-026 for manifest-declared skill action discovery and sync invoke.
- `crates/octos-cli/src/api/ui_protocol.rs::APPUI_EXTRA_METHODS`.
- `crates/octos-core/src/ui_protocol.rs::UI_PROTOCOL_NOTIFICATION_METHODS`.
