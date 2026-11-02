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
record version fails the entire `session/hydrate` RPC. The tolerant startup/replay
reader is not used to construct this authoritative replacement. No hot-ring-only
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
read limits, not changes to ledger retention, startup recovery, or writer behavior.

Exceeding a limit returns JSON-RPC `INTERNAL_ERROR` (-32603) with
`data.kind = "hydrate_references_unavailable"`, `reason`, `limit` (bytes or count,
null for non-budget failures), and `retryable`. Reasons are `read_failed`,
`invalid_record`, `scan_files_limit`, `scan_bytes_limit`, `reference_bytes_limit`,
`mutation_count_limit`, `mutation_bytes_limit`, `response_bytes_limit`, and
`worker_failed`. Only `read_failed` is marked retryable; callers may retry after
the underlying storage problem is repaired. Budget failures do not promise that
retrying the identical request will succeed. This revision does not add pagination.

A mutation-bearing result is serialized with its exact JSON-RPC envelope and
must fit the 1 MiB frame limit. It never enters the generic string/array preview
truncator. If it cannot fit, the entire RPC fails with `response_bytes_limit`.
Clients keep their last successful Changes snapshot and surface the hydrate
error; they must not replace it with an empty or partial list. Existing clients
already do not commit a hydrate result when the RPC rejects. Requests excluding
messages do not scan these references, and rollback continues to omit them.

## Validation

Cover cold recovery outside the hot ring, captured head and workspace
isolation, hydrate include/after behavior, and profile denial. Exercise real
file editing, session switching and page reload in the browser.

Follow-up regression coverage measures actual handler/frame latency and returned
counts for 1,000/20,000/80,000 notices in a durable trimmed-ring ledger, and tests
hot/cold count and escaped-byte limits, read/corruption failures with repair/retry,
and oversized combined responses without partial success. Timings are measurements
from a debug test build, not production latency guarantees.
