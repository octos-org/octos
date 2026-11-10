# UPCR-2026-RD1002E7E — Synchronous memory entry edit/delete

Status: proposed (implemented for review)
PR: TBD

## Decision record

This record is `UPCR-2026-RD1002E7E`. Cite it as `UPCR-2026-RD1002E7E` in the
spec, wire inventory, and Rust doc comments.

## Problem

The web memory panel (T-0547 findings) could not offer honest entry editing:
`memory/overview` exposes long-term memory only as one capped markdown blob,
and `memory/request`'s `correct`/`forget` merely ADMIT a note — the final text
is chosen later by a model merge, `status: "accepted"` says nothing about
completion, and an edit has no conflict precondition. Clients had to parse the
kernel's markdown format by hand and could never promise the user their own
text landed.

## Contract

Three new methods, all gated on the existing `auxiliary.rest_to_ws.v1` feature
(with optional `session_id` profile selection under `memory.session_scope.v1`,
same resolver and refusal rules as `memory/request`; refused for
session-ingress and host-managed external credentials; auth-bound, so omitted
from the stdio capability set):

- `memory/entries` — params `{ session_id? }`, result `MemoryEntriesResult`
  `{ entries: [{ id, text, version }], total, truncated }`. `text` is the full
  stored MEMORY.md block (id token and stamp included); `version` is the
  SHA-256 hex of that exact string. Truncation keeps WHOLE leading entries and
  flags `truncated` + `total`; nothing is cut mid-text. A missing or legacy
  (pre-id) file reports zero entries.
- `memory/edit` — params `{ session_id?, entry_id, version, content }`.
  Synchronous: on success the entry body IS `content`, verbatim, id unchanged,
  `(updated: <today>)` appended only when the body carries no stamp (the same
  rule a merge `update` op follows), previous file kept as `MEMORY.md.bak`.
  `content` obeys the `memory/request` 16 KiB cap and content guard, and must
  be a single entry block (no blank lines). No model call; independent of the
  memory-refresh toggle. Result `{ entry_id, version }` carries the NEW
  version.
- `memory/delete` — params `{ session_id?, entry_id, version }`. Synchronous,
  with the same effect as a completed id-bound forget: MEMORY.md removal plus
  whole-block removal of archived copies naming the id, line scrubs of bank /
  daily-note copies, `.bak`/`.prev` deletion, untrusted staging copies removed.
  Result `{ entry_id, version }` is the receipt of what was deleted.

Honest failures: malformed id/version/content → `invalid_params`; an id not
live in the CALLER's profile (including another profile's id) →
`resource_not_found` with `resource_type = "memory_entry"`; a stale `version`
→ the NEW code `resource_conflict` (`-32171`) with `data.kind =
"version_conflict"` and `data.current_version`, changing nothing. A failed
edit leaves the entry untouched (atomic publish); a delete that fails
mid-apply reports `internal_error` whose message states the live entry may
survive with some archived copies already removed, rather than claiming
all-or-nothing.

Mutual exclusion with background consolidation: a new per-profile flock
`memory/.write.lock` is held by each direct edit/delete for its whole
read-check-publish section AND by `run_consolidation` for its entire run
(snapshot through final apply). A direct mutation arriving mid-run waits and
then applies on the published state (its version precondition re-checked); a
consolidation plan built from a pre-mutation snapshot can never be published
over it — no lost edit, no resurrected entry. The pre-existing
`memory/.refresh.lock` remains the service-ownership lock and is unchanged.

Ordering with already-queued `memory/request` notes on the same id: staged
notes are not rewritten by direct operations. A queued `forget` for a directly
deleted id is consumed by the next consolidation's already-satisfied pre-pass;
a queued `correct` whose target was directly deleted is retired by the
engine's orphan-correction rule; a queued `correct` whose target was directly
edited still rides the next merge — user asks apply in submission order, the
direct edit first. All three behaviors pre-exist; this change only fixes the
file state they start from.

## Scope and verification

Engine-level tests (`memory_consolidate::direct`) cover verbatim edit,
archive/backup cascade on delete, not-found, stale-version conflict changing
nothing, single-block content rejection, shared-line scrubbing, and both
lock orderings against a live consolidation (delete during an in-flight merge
waits then conflicts honestly and succeeds on retry; a completed delete is
never resurrected by a later merge). RPC-level tests cover the session-scoped
happy path with no runtime/LLM (refresh independence), the conflict receipt,
cross-profile not-found, and malformed-id rejection. The spec §6 catalog, §10
error pins, and wire inventory list the new methods and `-32171`; the
supported-methods and capability gates advertise them only under
`auxiliary.rest_to_ws.v1`.
