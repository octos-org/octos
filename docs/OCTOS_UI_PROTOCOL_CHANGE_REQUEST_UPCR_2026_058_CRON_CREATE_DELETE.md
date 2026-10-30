# UPCR-2026-058 — Cron Create and Delete

Status: proposed (implemented for review)
Task: T-0014 PR2

## Methods

- `cron/create {session_id,schedule,message,deliver?,channel?,chat_id?,mode?,timezone?}` creates a job with a server-generated id and exact session/profile origin. `schedule` supports `At {at_ms}`, `Every {every_ms}`, and `Cron {expr}`; `mode` is `agent` or `notify`.
- `cron/delete {session_id,job_id}` is idempotent. Deleting a missing job returns `deleted: true`; a present job whose persistence fails is an error.
- Both use the existing `auxiliary.rest_to_ws.v1` gate and profile authorization. Host-managed external connections are denied by the existing management-method allowlist.

## Delivery and state

PR1's acknowledged delivery remains the commit point: one-shot deletion and `ok` happen only after the exact session's durable queue accepts the occurrence. Later turn failures do not re-fire it. Delivery refusal retains `last_error` and retries at most 5 times at 15-minute intervals, then retains and disables the job. A due job at startup is delivered once and marked late; recurring jobs do not replay every missed interval.

`cron/list` and create/toggle rows include `next_run`, `last_run`, `last_error`, and `enabled` in addition to the existing schedule/status fields, so the scheduled-task page can show retry and disabled state.

## Compatibility and persistence

Jobs are persisted by `CronService`, so live serve runtimes, gateway ownership checks, and file-backed stores share the same semantics. No new external management capability is exposed.
