# UPCR-2026-RDC3DC04D — A memory request consolidation cannot merge reports `failed`

Status: proposed (implemented for review)
PR: TBD

## Decision record

`UPCR-2026-RDC3DC04D`.

### Problem (octos#3253)

A host ask must never be silently dropped. Consolidation therefore rejects
any merge that leaves a host note unconsumed, and a host or user_request note
is never quarantined. When the merge model keeps declining one such note,
the note is retried on every fast-lane pass, about every 90 seconds. Each
retry is a full merge call that spends provider tokens, and it goes on
without end. Meanwhile the requester only ever saw `accepted`.

The rejection also fails the rest of the batch. Unrelated staging notes in
the same batch were counted as failed with it and quarantined after two such
passes.

On the reported instance the model was `zai-coding / glm-5.3`. Replaying the
merge prompt for a single host `user_request` showed the cause. The prompt
says every item goes to `consumed_ids` or to `dropped` with a reason, and
never says that a host note may not be dropped. glm-5.3 dropped a note that
read like a temporary release-check record ("no durable fact") in 2 of 2
runs. It saved ordinary content in 4 of 4.

### Decision

- **Prompt.** The merge policy now states that a host note may never be
  dropped, however temporary or trivial it looks. It must be honored by an op
  that cites it in `sources` and be listed in `consumed_ids`. With the same
  model, the same note was then saved in 3 of 3 runs, and the other contents
  in 6 of 6.
- **Cap.** An ask the model must honor is a host note other than a forget, or
  any user_request. Once such an ask has been in `STUCK_ASK_THRESHOLD` (3)
  consecutive failed merges, it is parked: the note moves to
  `memory/staging/failed/`, and `<stem>.json` beside it records `reason`,
  `attempts` and `failed_at`. It is no longer retried. It is kept, never
  deleted. Forget notes are unchanged: they are executed or parked
  Rust-side, without the model.
- **Backoff.** After a failed merge, the ask stays out of the batch for 5
  minutes, doubling after each further failure. Other staging merges in the
  meantime. A parked ask therefore cost at most three merge calls over about
  15 minutes.
- **Restarts.** `memory/staging/.consolidate_failures.json` now also records
  `last_failed_at` and `last_error` for each file. The cap and the backoff
  are read from it, so both hold across restarts. A bare count written by an
  older build still loads.

### Wire

`memory/request` (UPCR-2026-052) gains one status and one field:

- `MemoryRequestStatus` gains `"failed"`.
- `MemoryRequestResult` gains an optional `reason`. It is present only with
  `failed`.

When the same `request_id` is sent again after its request was parked, the
result is `status: "failed"` with a `reason` safe to show. The reason
carries no paths, note content or model output, for example "the memory
model did not save it in 3 attempts; the request is kept but no longer
retried". Retry rules are unchanged: the same id with a different payload is
still `request_id_conflict`. A client that wants to try again sends a new
`request_id`.

### Compatibility

- A request that merges, and every first submission, still returns
  `accepted` byte for byte. `reason` is omitted when absent.
- `failed` appears only on a resend of a request that previously would have
  retried forever. Octoscode Web reads `status` as a string and shows it, so
  it shows "failed". A client that decodes the status as a closed set must
  treat an unknown value as "not accepted".
- No new capability is needed. The value describes a state that did not
  exist before: such a request previously stayed `accepted` while it
  retried forever.

### Not in this change

- A push event when a request fails. Clients learn of it by resending the id.
- A way to retry a parked request in place, or to list parked requests.
