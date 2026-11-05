# UPCR-2026-069 — Recover session change preview references

Status: proposed (implemented for review)
Task: T-0062
Capability: `state.session_file_mutations.v1`

## Problem

File-mutation progress and diff previews are already durable, but hydrate
returns neither the mutations nor their preview IDs. A client loses its
Changes list on selection or reload even though `diff/preview/get` still
serves the preview. Tool-card write paths cannot recover those references.

## Contract

Add optional `file_mutations: UiFileMutationNotice[]` to `session/hydrate`
when `include` requests messages (including the default). This is retained
actual mutation history in ascending ledger order through the captured
hydrate cursor, independent of the request's `after`. Read beyond a trimmed
hot ring from existing retained logs; exclude later events and other
session/workspace streams. No pending diff proposal is counted as a mutation.
No file content, inferred paths, or regenerated diff is added. Clients fold
this snapshot and then continue with live progress after the cursor.

The field is additive and ungated, with a discoverable feature like tool
write paths. Old clients ignore it; old servers omit it, so clients preserve
live observations when absent. Existing hydrate authorization, retention,
and diff-preview lifetime apply. Conversation-only rollback omits the field:
rewinding a transcript does not undo filesystem mutations.

### Complete-or-error hydration

The replacement snapshot must be complete within the retained-history contract.
An unreadable or missing required log, invalid UTF-8/JSON, or unsupported ledger
record version fails the entire `session/hydrate` RPC. The bounded retained-reference reader constructs this authoritative replacement
independently of the runtime projection cache. No hot-ring-only
fallback or partial `file_mutations` array is returned on failure. Valid removed
legacy `message_persisted` records still reserve their cursor space but do not
represent file changes.

Retained-reference hydration reads at most 32 log files / 64 MiB of log bytes,
which accommodates the default five 10 MiB rotated logs, with the byte bound
enforced during reads as well as metadata preflight. Selected
references have an additional 8 MiB serialized budget. The mutation snapshot is
limited to 1,024 notices and 256 KiB serialized JSON, including escaping and array
delimiters. Canonical message identities and mutations share one scan, run on the
blocking pool rather than the asynchronous request executor. These are hydrate
read limits, not changes to ledger retention.

Lazy recovery removes only an unterminated last record in the latest log before
replaying or appending. Newline-terminated corrupt records and older log files
are not repaired. A confirmed malformed-history failure remains latched until
restart: even if storage later looks like an unterminated tail, a same-process
retry cannot truncate it to bypass the latch.

Read-only I/O failures, including directory/index errors and failed tail
inspection or truncation, return explicit `read_failed` errors. Hydrate and
pre-append recovery may retry these reads after storage repair without restarting.
An unreadable existing history is never treated as a new empty session; no event
is admitted before recovery succeeds. A vanished or dangling log is a read
failure, not an absent session directory. A successful recovery reconciles crashed
lifecycle rows before admitting new work, retaining original `recorded_at` values.
Complete projections still use the existing snapshot cache and cadence.

Durable append failures follow the write-ahead contract introduced by #3000.
A failed or ambiguous write returns an error before ACK, ring/live publication,
cursor advancement, or terminal completion. The session remains unavailable
until restart, including after storage repair or idle/LRU eviction. Partial
appends attempt rollback to their pre-write length under the writer lock; failed
rollback retains that boundary and cannot be bypassed by another append or log
rotation. Successful rollback does not clear the failed-write latch. Restart
reconciles actual persisted history, including a complete record written before
an ambiguous error, without reusing its cursor. There is no RAM-only success or
failed-write reference cache, and a turn affected by ENOSPC is not promised to
complete. This preserves the existing page-cache durability policy; it does not
add per-event fsync. Malformed-history and durable-write failures do not prevent
unrelated sessions from being read.

Exceeding a limit returns JSON-RPC `INTERNAL_ERROR` (-32603) with
`data.kind = "hydrate_references_unavailable"`, `reason`, `limit` (bytes or count,
null for non-budget failures), and `retryable`. Reasons are `read_failed`,
`invalid_record`, `scan_files_limit`, `scan_bytes_limit`, `reference_bytes_limit`,
`mutation_count_limit`, `mutation_bytes_limit`, `response_bytes_limit`, and
`worker_failed`, and `durability_failed`. Only `read_failed` is marked retryable; callers may retry after
the underlying storage problem is repaired. Budget failures do not promise that
retrying the identical request will succeed. This revision does not add pagination.

Conversation records use the existing string/array preview truncator regardless
of whether Changes are present. The exact JSON-encoded mutation array size is
reserved in the 1 MiB frame budget, including escaping and the JSON-RPC envelope.
The mutation snapshot is restored unchanged after record previews are generated.
If the combined response still cannot fit, the entire RPC fails with
`response_bytes_limit`; even 1,024 mutations within the array byte limit are not
guaranteed to fit every response. No partial mutation snapshot is returned.
Clients should keep their last successful view and surface the hydrate error;
client error-state preservation requires separate frontend work and is not a
verified consequence of this server change. Requests excluding messages do not
scan these references, and rollback continues to omit them.

## Validation

Cover cold recovery outside the hot ring, captured head and workspace
isolation, hydrate include/after behavior, and profile denial. Exercise real
file editing, session switching and page reload in the browser.

Follow-up regression coverage measures actual handler/frame latency and returned
counts for 1,000/20,000/80,000 notices in a durable trimmed-ring ledger, and tests
hot/cold count and escaped-byte limits, read/corruption failures with repair/retry,
and oversized combined responses without partial mutation success. Long-record
coverage includes 40/60 tool outputs, two 592 KiB prompts, and JSON escaping;
record previews must fit alongside an unchanged mutation snapshot. Timings are measurements
from a debug test build, not production latency guarantees.

Restart coverage includes histories beyond the 4,096-record hot ring, real writer
partial writes, same-process permission repair/retry, and negative controls for
corrupt middle records and older logs. Completed log records and existing long
record/frame budgeting behavior remain intact.

Integration coverage preserves #3000's no-ACK/no-live/terminal/cursor regressions,
adds durable-write latches across idle/LRU eviction, and proves that tail repair
cannot bypass a malformed-history latch. Read-only faults retry in the same
process; write faults remain rejected until restart, which restores all committed
references without including rejected RAM-only events.
