# UPCR-2026-058 — Cron Create and Delete

Status: proposed (implemented for review)
Task: T-0014 PR2

## Methods

- `cron/create {session_id,schedule,message,deliver?,channel?,chat_id?,mode?,timezone?}` creates a job with a server-generated id and exact session/profile origin. `schedule` supports `At {at_ms}`, `Every {every_ms}`, and `Cron {expr}`; `mode` is `agent` or `notify`.
- `cron/delete {session_id,job_id}` is idempotent after successful scope resolution. Deleting a missing job returns `deleted: true`; a present job whose persistence fails is an error.
- `cron/list {session_id?}` and `cron/toggle {job_id,enabled,session_id?}` accept the same optional session scope. Omitting it preserves the legacy identity/tenant behavior.
- All four use the existing `auxiliary.rest_to_ws.v1` gate and one effective-profile rule. Host-managed external connections are denied by the existing management-method allowlist.

## Effective profile and validation

1. A resolved tenant host is authoritative. A session prefix that disagrees with it is denied, including an unknown prefix; tenant authorization is not bypassed.
2. A user identity remains confined to its own profile. A session belonging to another profile is denied.
3. An admin token with a profile-prefixed session operates in that registered profile; an unknown profile returns `profile_not_found`. An unprefixed session keeps the admin default.
4. Create/delete/list/toggle persist and read only this effective profile. The original session id remains in `origin.session_id`; `origin.profile_id` records the effective profile.

For `Cron {expr}`, create requires exactly six fields with seconds first (for example `0 0 9 * * *`). Empty, malformed, five-field, and seven-field expressions are rejected with HTTP/RPC 400 before persistence. A supplied timezone must be a valid IANA name or create returns 400; it never silently falls back to UTC. Existing stored jobs are not scanned or migrated, and their historical `compute_next_run` behavior is unchanged.

## Delivery and state

PR1's acknowledged delivery remains the commit point: one-shot deletion and `ok` happen only after the exact session's durable queue accepts the occurrence. Later turn failures do not re-fire it. Delivery refusal retains `last_error` and retries at most 5 times at 15-minute intervals, then retains and disables the job. A due job at startup is delivered once and marked late; recurring jobs do not replay every missed interval.

`cron/list` and create/toggle rows include `next_run`, `last_run`, `last_error`, and `enabled` in addition to the existing schedule/status fields, so the scheduled-task page can show retry and disabled state.

## Compatibility and persistence

Jobs are persisted by `CronService`, so live serve runtimes, gateway ownership checks, and file-backed stores share the same semantics. No new external management capability is exposed.
