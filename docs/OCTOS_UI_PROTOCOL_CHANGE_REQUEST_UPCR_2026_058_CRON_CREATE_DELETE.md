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

For `Cron {expr}`, create requires exactly six fields with seconds first (for example `0 0 9 * * *`). Each whitespace token must be exactly one lexical cron field (wildcard, value/name, range, list, or step); glued input that the parser could reinterpret as another field is rejected. The expression must also have no explicit year (`parsed.years().is_all()`) and at least one upcoming occurrence in the requested timezone. Empty, malformed, five-field, seven-field, glued-field, explicit/past-year, and impossible-date expressions are rejected with HTTP/RPC 400 before persistence; `cron.json` is not created or rewritten, so a rejected job cannot later be enabled with `enabled:true,next_run:null`. A supplied timezone must be a valid IANA name or create returns 400; it never silently falls back to UTC. The upcoming check uses that requested timezone, including DST gaps, rather than a UTC-only approximation. Product-supported runs end on 2099-12-31 in every timezone: a valid expression whose next result is later (including a natural DST gap that resolves into 2100) or cannot be computed is rejected with the same 400 and no persistence. This is an explicit product year limit, not a claim that year 2100 is a cron-library sentinel. Existing stored jobs are not scanned or migrated, and their historical `compute_next_run` behavior is unchanged.

## Delivery and state

PR1's acknowledged delivery remains the commit point: one-shot deletion and `ok` happen only after the exact session's durable queue accepts the occurrence. Later turn failures do not re-fire it. Delivery refusal retains `last_error` and retries at most 5 times at 15-minute intervals, then retains and disables the job. A due job at startup is delivered once and marked late; recurring jobs do not replay every missed interval.

`cron/list` and create/toggle rows include `next_run`, `last_run`, `last_error`, and `enabled` in addition to the existing schedule/status fields, so the scheduled-task page can show retry and disabled state.

## Compatibility and persistence

Jobs are persisted by `CronService`, so live serve runtimes, gateway ownership checks, and file-backed stores share the same semantics. No new external management capability is exposed.

## Weekday numbering correction

New OUP creates, CLI `--cron`, and the agent cron tool accept standard
weekday numbers: 0/7=Sunday, 1=Monday, through 6=Saturday. `MON-FRI` equals
`1-5`; names, ascending ranges, lists and positive steps compose in that input
domain. Expand the inclusive range before merging Sunday aliases: `5-7` selects
Friday/Saturday/Sunday, `0-7/2` and `*/2` select Sunday/Tuesday/Thursday/Saturday,
`1-5/2` and `MON-FRI/2` select Monday/Wednesday/Friday. A point with a step runs
through 7; names use Sunday=0, so `MON-SUN` is descending and rejected (use
`1-7` or `MON-SAT,SUN`). Malformed weekdays return `invalid_cron_expr` / HTTP 400
before writing a job. See the [cron weekday convention](https://man7.org/linux/man-pages/man5/crontab.5.html).

This changes weekdays only: seconds remain the first field, other fields retain
the cron library's grammar and matching rules, and day-of-month and weekday
still both constrain execution. OUP creation normalizes the weekday before the
existing six-field, lexical-field, timezone and 2099 upcoming-occurrence checks;
the Every/At bounds above remain in force. CLI and agent-tool expressions keep
their existing optional-year and alias grammar.

Creation converts weekdays to names (or `*` for all days). The existing
`CronStore` version 1 and scheduler are unchanged; a numeric expression already
on disk still means 1=Sunday through 7=Saturday. No automatic migration or
on-load write occurs. A later toggle/save also preserves that expression.
List/create/toggle presentation expands stored weekdays into names, so old
`1-5` displays `SUN,MON,TUE,WED,THU` and copying a rendered row preserves its
execution days. Invalid stored expressions are visibly labelled in the row;
this does not repair or enable them. Clients must display the returned expression,
not reinterpret cached legacy numbers. New named storage is also understood by
older kernels on rollback. Embedded Rust callers constructing `CronSchedule::Cron`
directly keep storage semantics; `CronSchedule::from_standard_cron` is the explicit
user-input boundary. Natural-language weekly schedules now emit names directly.
