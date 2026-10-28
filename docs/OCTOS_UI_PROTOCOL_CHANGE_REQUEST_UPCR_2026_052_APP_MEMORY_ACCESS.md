# UPCR-2026-052: application memory requests and document promotion

## Header

- Request id: `UPCR-2026-052`
- Date: 2026-10-20
- Status: proposed (implemented in PR #2848; pending upstream review)
- Issue: application-access follow-up to #2355
- Target protocol: `octos-ui/v1alpha1`
- Scope: additive typed commands `memory/request` and `memory/promote`

## Problem

Applications that run their own skill/model pipeline need to submit explicit
user memory requests and nominate existing application documents without an
Octos turn. Existing memory processing already supports these operations
internally, but OUP does not expose the authenticated application contract.
The application must not bypass profile authorization or become a second
memory consolidator merely to reach that processing.

## Authorization and negotiation

Both methods require `auxiliary.rest_to_ws.v1` and resolve storage through
the existing authenticated `/api/my/*` profile resolver used by `memory/ingest`.
The resolved profile owns the request receipts, staging notes, live entries
and Recall records. Params do not accept a profile, identity, origin, local
path or authorization override; unknown fields are rejected.

| Connection / condition | Contract for both methods |
|---|---|
| Authenticated WebSocket with the auxiliary feature negotiated | Allowed subject to existing profile resolution and method validation |
| Auxiliary feature not negotiated | `method_not_supported`; no handler admission |
| Missing authenticated identity | `auth_unavailable`; the WebSocket auth-expiry path also closes with code 1008 |
| Session-ingress credential, even with the feature negotiated | Rejected as profile-global operations; omitted from its callable capabilities |
| Host-managed external connection | Not in the external-client allowlist; rejected |
| Current stdio transport without identity | Not advertised; direct dispatch cannot bypass the handler's identity check |
| Target entry exists only in another profile | `resource_not_found` for `memory_entry` in the caller's profile |

A source label is a selection filter, not an authorization credential.
No session parameter is provided to turn a session-scoped token into
profile-global access. Existing memory methods and CLI commands are unchanged.

## `memory/request`

Params: `MemoryRequestParams` — `{ request_id, operation, content?, entry_id? }`.

- `request_id`: 1–128 ASCII letters/digits or `-_.:`; scoped to the resolved
  profile, not a global application namespace.
- `remember`: non-blank content, no entry id.
- `correct`: non-blank content and the id of a current live `MEMORY.md` entry.
- `forget`: a current live entry id only; content, including an empty string,
  is invalid. There is no free-text deletion selector.
- Entry ids are `^m` followed by six `[a-z2-7]` characters, never paths.
- Content is at most 16,384 UTF-8 bytes and passes the existing note content
  guard. Oversized content is rejected rather than silently truncated.

The host creates `user_request`, targeted `correction` (`replaces_id`), or
id-bound `forget` notes. The handler neither starts a turn nor calls a model;
the existing profile background consolidator owns actual memory changes.
Applications submit explicit user requests, not model/app observations
presented as user-authorized changes.

Result: `MemoryRequestResult` —
`{ request_id, operation, status: "accepted", refresh_enabled }`.

`accepted` means durable admission, not successful consolidation.
`refresh_enabled` reports configuration, not worker health, a completion
result, or a time guarantee. Disabled refresh leaves work staged.

### Replay, conflict and interrupted publication

1. The host checks an existing receipt before target existence. Replaying an
   accepted forget remains valid after its target has been deleted.
2. The same id and payload reuse acceptance; a different operation, target or
   content returns `invalid_params` with `data.kind = "request_id_conflict"`.
   Payload comparison is not semantic equivalence of different prose.
3. Admission is serialized by a cross-process `fs2` file lock. A private
   prepared note is synced before the atomic receipt publication, then renamed
   into the existing staging directory. Directory syncs are performed on Unix.
   Receipts retain a payload fingerprint and generated note id, not note text.
4. Failure before a receipt is published may leave a private prepared file;
   retry can replace it. Once a receipt exists, retry publishes a still-prepared
   note. A note already published and consumed is never recreated from the
   request payload. Reopening the store preserves replay/conflict handling.
5. Receipts have no automatic expiry in this slice. Neither receipt existence
   nor staging disappearance proves the operation completed.

Before model merging, consolidation retires a host correction only when its
explicit target is absent from live entries, archives and pending-confirm
candidates. It records the reason, retains the receipt, and preserves unrelated
requests. This cleanup also runs when model merging is disabled; it does not
promise to prevent all in-flight regeneration of forgotten information.

Errors follow the existing memory-method taxonomy: `auth_unavailable`,
`invalid_params`, `resource_not_found` for `memory_entry`,
`runtime_unavailable`, and `internal_error`. Storage failures do not expose
local paths, note text or internal error details.

## `memory/promote`

Params: `MemoryPromoteParams` —
`{ sources?: string[], min_visits?: number, limit?: number }`.

- Sources default to empty, meaning all sources within the resolved profile.
  At most 32 labels; each is 1–128 UTF-8 bytes without surrounding whitespace
  or control characters. Source filtering precedes ranking and limiting.
- `min_visits` is a u32, defaults to 2, and is floored at one actual document
  load even if the caller supplies 0.
- `limit` defaults to 20; explicit values must be in 1–100.
- Only eligible, unpromoted Document records are selected. Existing heat
  ordering and once-per-record behaviour remain; updating a promoted document
  does not automatically nominate it again.

Result: `MemoryPromoteResult` — `{ record_ids, count, refresh_enabled }`.
Returned ids identify notes staged for the background consolidator, not
completed Knowledge writes. Zero eligible/staged records is a valid empty
result. Notes rejected by the existing content guard are not counted.

The existing nomination function stages title, abstract and provenance, not
the stored body. The handler makes no model call and starts no scheduler.
The app/controller explicitly invokes it after real retrieval; fake loads
must not be used to bypass eligibility. Background refresh remains subject
to existing configuration, budget and failure behaviour.

Concurrency uses the in-process Tokio promotion mutex on the shared
`RecallStore`, held through selection, staging and marking. This is distinct
from the cross-process request-admission file lock above. Staging and marking
are still separate writes; process failure between them is not a crash-atomic
or exactly-once promotion guarantee. Errors use existing auth/runtime shapes,
`invalid_params` for validation, and `internal_error` for persistence failure.

## Compatibility, scope and verification

The additions introduce typed DTOs, command variants, codecs, method inventory
and dispatcher wiring. They introduce no notification, completion event or
new ledger classification. Existing callers keep their promotion defaults.
The normative field contract is in the [OUP specification](../api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md).

Non-goals: per-request completion queries, receipt expiry, archive-only or
free-text mutation targeting, automatic document nomination, a permanent app
record store, product-specific teacher policy, and general protection against
old in-flight extraction results. These methods do not strengthen the replay
contract of `session/append_message`.

Existing tests in this PR cover:

- `octos-memory/src/memory_request.rs`: replay after consumption/reopen,
  payload conflict, interrupted publication recovery, concurrent admission,
  validation and content-free receipts.
- `octos-agent/src/memory_index.rs`: source filtering before the limit and
  serialization of concurrent nomination through the shared store.
- `octos-core/src/ui_protocol_tests.rs`: command codecs, method inventories
  and negotiation gates.
- `octos-cli/src/api/ui_protocol_memory_request_tests.rs`: authenticated
  handlers, two-profile isolation, missing targets, replay/conflict,
  validation, source-filtered promotion, and restricted capability checks.
- Existing auxiliary, session-ingress, host-managed and stdio inventory tests:
  dispatch and permission integration.
- `octos-cli/src/memory_consolidate/app_pipeline_tests.rs`: actual pipeline
  boundaries and targeted correction retirement, including no-model cleanup,
  retained archive targets, unrelated work and replay after retirement.

Test providers are scripted; this is not real-model quality or power-loss
validation. Executed commands and the independent-review boundary are recorded
in [the implementation report](APP_MEMORY_ACCESS_IMPLEMENTATION.md).
