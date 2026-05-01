# Octos UI Protocol Change Request: `session/hydrate` Command

## Header

- Request id: `UPCR-2026-009`
- Title: Add `session/hydrate` RPC command for authoritative chat-state reload
- Author: 5-day structural plan, Day 1 (coding-green)
- Date: 2026-04-30
- Target protocol: `octos-ui/v1alpha1`
- Status: proposed
- Related issues: `#738`, `#740`, `#742` (thread-binding bug chain — persistence-side
  reload misbinding observed in this week's web-client soak)
- Related plan: `/tmp/octos-architecture-FINAL.md` § Day 1 (UPCR-2026-009)

## Summary

This change request adds one additive JSON-RPC command method, `session/hydrate`,
to the AppUi protocol so clients can fetch the authoritative chat state of a
session — messages, threads, turns, and pending approvals — in a single
round-trip on reload. Today the only reload entry point is `session/open`,
which advertises capabilities and replays the event ledger from a cursor but
does **not** return the canonical message/thread/turn projection. Clients are
forced to either reconstruct the chat state from a stream of `message/delta`
notifications replayed from cursor 0 (slow, lossy if the ledger is compacted)
or fall back to the legacy REST `GET /api/sessions/:id/messages` endpoint
(out-of-band, unaware of UPCR-2026-007 capability negotiation). This UPCR
closes that gap by giving clients a typed snapshot RPC gated behind the new
`state.session_hydrate.v1` capability flag.

The change is strictly additive: no existing method, notification, payload,
enum variant, capability bit, or protocol identifier is modified.

## Motivation

The 5-day structural plan (`/tmp/octos-architecture-FINAL.md`) traces the
thread-binding bug chain `#738 → #740 → #742` to a single root cause: clients
have no authoritative way to ask the server "what is the canonical state of
this session right now?". The web SPA's reload path infers chat state from a
mix of REST snapshots, SSE replay, and locally cached optimistic bubbles. Each
inference site is a place a real bug has shipped this quarter:

- **#738**: a persisted assistant row with a stale `thread_id` was rendered
  under the wrong question on reload because the SPA could not distinguish
  "the row the server just wrote" from "the row I optimistically rendered".
- **#740**: the same row was further misbound because the SPA could not see
  the canonical `(turn_id, thread_id, seq)` tuple the server actually
  committed — only the wire-side message it streamed.
- **#742**: the tactical fix pre-stamps `thread_id` at persistence time, but
  there is still no client-side way to verify the stamp landed.

`session/open` itself does not solve this:

- Its result type `SessionOpenResult { opened: SessionOpened { session_id,
  active_profile_id, workspace_root, cursor, panes } }`
  (`crates/octos-core/src/ui_protocol.rs:1342-1352`) carries **no** message,
  thread, turn, or approval payload.
- Its `cursor` field is a resume point for the event stream, not a snapshot
  of state.
- Its primary job is capability negotiation (see accepted `UPCR-2026-007`)
  and lifecycle: it must stay fast and small so it can run on every WebSocket
  reconnect.

A separate `session/hydrate` command lets clients pay the snapshot cost only
when they need it (initial load, full reload, debugger inspection) without
bloating `session/open`.

## Change Type

Additive method.

One new JSON-RPC command method on the existing AppUi v1alpha1 protocol. No
new notifications. No existing method, notification, required field, enum
variant, or capability flag is modified. One additive feature flag
(`state.session_hydrate.v1`) is added so clients can negotiate availability.

## Wire Contract

Affected wire surface — strictly additive:

- Capability payload: `UiProtocolCapabilities` (new feature flag entry,
  full-protocol method-set entry)
- Capability feature registry: `state.session_hydrate.v1`
- Command method: `session/hydrate` (new)
- Command params: `SessionHydrateParams` (new)
- Command result: `SessionHydrateResult` (new)

No existing command method, notification, params, results, or enum variants
are modified by this UPCR.

### `session/hydrate`

Purpose:

- Return the authoritative chat-state projection for one session in one
  round-trip: messages, thread graph, turn lifecycle states, and pending
  approvals. Primary consumer: web SPA reload, TUI session-restore, harness
  test fixtures.

Params:

```json
{
  "session_id": "local:demo",
  "after": { "stream": "session", "seq": 0 },
  "include": ["messages", "threads", "turns", "pending_approvals"]
}
```

- `session_id` (required): canonical session identifier.
- `after` (optional `UiCursor`): when present, hydrate returns only items with
  cursor strictly greater than `after`. Useful for incremental rehydration
  when the client retains a partial cache. Absent = full hydrate from the
  beginning.
- `include` (optional `string[]`): selection set restricting the response to
  the requested projections. Empty or absent = include all four. Unknown
  tokens are silently dropped (matches the `X-Octos-Ui-Features` precedent
  from `UPCR-2026-007`). Recognised tokens:
  - `messages` — populates `messages`.
  - `threads` — populates `threads`.
  - `turns` — populates `turns`.
  - `pending_approvals` — populates `pending_approvals`.

Result:

```json
{
  "session_id": "local:demo",
  "cursor": { "stream": "session", "seq": 142 },
  "messages": [
    {
      "seq": 17,
      "role": "user",
      "client_message_id": "01900000-0000-7000-8000-000000000001",
      "thread_id": "thread-1",
      "turn_id": "01900000-0000-7000-8000-000000000010",
      "content": "hello"
    }
  ],
  "threads": [
    {
      "thread_id": "thread-1",
      "root_seq": 17,
      "root_client_message_id": "01900000-0000-7000-8000-000000000001",
      "turn_id": "01900000-0000-7000-8000-000000000010",
      "message_seqs": [17, 18, 19],
      "status": "completed"
    }
  ],
  "turns": [
    {
      "turn_id": "01900000-0000-7000-8000-000000000010",
      "thread_id": "thread-1",
      "state": "completed",
      "started_at": "2026-04-30T12:00:00Z",
      "completed_at": "2026-04-30T12:00:05Z"
    }
  ],
  "pending_approvals": []
}
```

- `cursor` (required `UiCursor`): the head cursor of the session at the
  moment the snapshot was assembled. Clients should use this as the
  baseline for any subsequent `after` request and as the resume point for
  the live event stream — the snapshot is an atomic projection up to and
  including this cursor.
- `messages` (required `Message[]`, may be empty): the canonical chat rows.
  Each entry carries `(seq, role, thread_id, turn_id, client_message_id?,
  content)` matching the existing on-disk projection. Omitted from the
  response only when `include` excludes `messages`; never `null` when
  included.
- `threads` (required `Thread[]`, may be empty): the thread graph (see
  `UPCR-2026-010` for the same shape). Bundled here so a single hydrate
  round-trip is sufficient for full SPA rehydration.
- `turns` (required `TurnSummary[]`, may be empty): one entry per known
  turn for the session. State enum values match `UPCR-2026-011`'s
  `turn/state/get` registry.
- `pending_approvals` (required `PendingApproval[]`, may be empty): the
  set of approvals currently in `Awaiting` state, with the same payload
  shape `approval/requested` carries (governed by `UPCR-2026-001`). This
  lets clients re-render the approval UI on reload without replaying the
  ledger.

Sections that the client did not request via `include` are omitted from the
response object entirely (not present as `null`). The `cursor` and
`session_id` fields are always present.

## Error Model

The new command returns errors from the existing v1 taxonomy:

- `unknown_session` — server has no session with the given `session_id`, or
  the session is scoped to a different connection profile than the request.
- `cursor_out_of_range` — the supplied `after` cursor addresses a position
  beyond the current ledger head. Echoes ledger head per the existing
  cursor-range error shape (`crates/octos-core/src/ui_protocol.rs:419-433`).
- `cursor_invalid` — the supplied `after` cursor is malformed or wrong-stream.
- `invalid_params` — params failed structural validation. Includes:
  - `data.kind = "include_too_large"` when the `include` array exceeds 32
    entries (defensive against pathological requests).
- `runtime_unavailable` — server has no chat-state projection wired for this
  deployment (e.g. lite/embedded build that never persisted messages).
  Returned with `data.kind = "runtime_unavailable"`.

A `session/hydrate` request for a known but empty session returns success
with `messages: []`, `threads: []`, `turns: []`, `pending_approvals: []`,
and the current head cursor. Empty is not an error.

No new error categories are introduced.

## Compatibility

- Old clients that never request `state.session_hydrate.v1` and never send
  `session/hydrate` are unaffected. They continue to use `session/open` for
  capability negotiation and either replay from cursor or call the legacy
  REST endpoint for chat state.
- Old servers that have not implemented this method reject incoming
  `session/hydrate` requests with the existing `method_not_supported` error
  (`UI_PROTOCOL_FIRST_SERVER_METHODS` membership check). Clients should
  detect this via capability negotiation rather than by trial-call.
- Clients that exhaustively match on `UiCommand` or `UiResultKind` and have
  not been recompiled against the new enum variants will fail to deserialize
  a message carrying the new method. This is the standard
  forward-compatibility behaviour for any added method, acknowledged by
  spec § 4.1.
- The legacy REST `GET /api/sessions/:id/messages` endpoint stays live for
  the deprecation window; this UPCR does not delete it. The web SPA migration
  in PR J of the structural plan switches from REST to `session/hydrate` only
  when capability negotiation succeeds.
- No new protocol identifier is required because the change is additive.

## Capability Negotiation

New feature flag:

- `state.session_hydrate.v1`

Servers advertise it through `UiProtocolCapabilities.supported_features`
when the chat-state projection is available. The flag is included in
`UiProtocolCapabilities::full_protocol()` and in the first server slice's
supported method set. Clients that want to depend on `session/hydrate`
should request the feature through the existing `X-Octos-Ui-Features`
header or the `ui_feature` / `ui_features` query parameters, then read
back the negotiated set from `SessionOpened.capabilities` (per
`UPCR-2026-007`).

If a client sends `session/hydrate` to a server that does not advertise
the feature, the server returns the existing `method_not_supported` error.
A client that observes `state.session_hydrate.v1` absent from
`SessionOpened.capabilities.supported_features` should fall back to the
legacy REST endpoint or to ledger replay from `session/open`'s cursor.

## Tests

- `crates/octos-core/src/ui_protocol.rs`:
  - `session_hydrate_command_round_trips_through_json_rpc` — round-trips
    `session/hydrate` request through the JSON-RPC envelope including the
    optional `after` and `include` fields.
  - `session_hydrate_result_omits_excluded_sections` — golden: an `include`
    of `["messages"]` produces a result with `messages` populated and
    `threads` / `turns` / `pending_approvals` absent (not `null`).
  - `session_hydrate_result_round_trips_with_full_payload` — golden:
    full-shape result round-trips through serde unchanged.
  - `typed_rpc_results_map_from_methods_and_round_trip` — extended with
    `SessionHydrateResult` golden coverage.
  - `full_protocol_capabilities_advertise_state_session_hydrate` — asserts
    the new feature flag and method are advertised in `full_protocol()`.
  - Capability-set golden tests updated to include the new method and the
    `state.session_hydrate.v1` feature literal.
- `crates/octos-cli/src/api/ui_protocol.rs`:
  - `appui_session_hydrate_returns_full_chat_state` — verifies the handler
    returns messages, threads, turns, and pending approvals for a
    populated session.
  - `appui_session_hydrate_honours_after_cursor` — verifies items with
    cursor ≤ `after` are excluded from the response.
  - `appui_session_hydrate_honours_include_filter` — verifies an `include`
    of `["threads"]` returns only the thread graph.
  - `appui_session_hydrate_returns_empty_for_unknown_session` — verifies
    the unknown-session path returns `unknown_session` rather than an
    empty success.
  - Routing tests extended with `session/hydrate` requests.
- `e2e/tests/`:
  - `m9-protocol-session-hydrate.spec.ts` — end-to-end: open a session,
    drive a turn, disconnect WS, reconnect, call `session/hydrate`, assert
    the projection matches the live state observed pre-disconnect.

## Rollout Plan

1. Land the protocol constants, params/result types, command/result enums,
   capability flag, and golden tests in `octos-core`.
2. Land the server handler `handle_session_hydrate` in `octos-cli`, including
   the chat-state projection helper that reads from
   `Session::messages()` / `Session::threads()` / the active-turns registry
   / the approval store. Routing tests added.
3. Update `api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md` § 4.1, § 6, and § 7
   to reference `UPCR-2026-009`.
4. Web SPA migration (Day 4 PR J) consumes `session/hydrate` for reload
   instead of the REST endpoint.
5. Deprecation window: keep `GET /api/sessions/:id/messages` live through
   the next minor release. Removal is tracked separately and requires its
   own UPCR if the wire surface changes.

No client renegotiation is required for clients that do not consume the new
method.

## Risks

- **Snapshot cost**: hydrating a long session may produce a large response.
  Mitigation: the `after` cursor and `include` filter let clients page or
  scope the request. Large-session pagination beyond `after` is out of
  scope for this UPCR — if it proves necessary, a follow-up UPCR can add
  `limit` semantics. The 32-entry `include` cap is defensive against
  pathological requests, not a per-section size cap.
- **Snapshot/stream divergence**: if a turn completes between the snapshot
  assembly and the client's resume, the client must reconcile the snapshot
  cursor with the live stream's cursor. This is the same reconciliation
  rule the `session/open` cursor already implies; the UPCR does not change
  it.
- **Bundled projections**: bundling `threads` / `turns` / `pending_approvals`
  into one RPC means a future schema change to any one of them affects the
  hydrate result shape. Mitigation: each section is governed by its own
  UPCR (`UPCR-2026-010` for threads, `UPCR-2026-011` for turns,
  `UPCR-2026-001` for approvals), and `include` lets clients opt out of
  sections they do not need. The bundled response is a convenience, not a
  coupling — a client may always issue separate `thread/graph/get` and
  `turn/state/get` requests instead.

## Decision

Proposed by: 5-day structural plan, Day 1.

Decision notes: Pending review. Closes the persistence-side reload
misbinding gap exposed by `#738` / `#740` / `#742` by giving clients a
single typed RPC for authoritative chat state on reload. The new
capability flag `state.session_hydrate.v1` lets clients depend on the
method only when negotiated. Bundled with `UPCR-2026-010` /
`UPCR-2026-011` / `UPCR-2026-012` as the four state-control primitives
the structural plan identifies as missing from v1.

### Out of scope

- **Pagination beyond `after`**: large-session pagination (`limit` / page
  tokens) is deferred. The `after` cursor is sufficient for incremental
  rehydration; a separate UPCR can add `limit` if it proves necessary.
- **Server-side caching of snapshots**: the handler may cache snapshots
  internally (e.g. memoize per `session_id` until the next cursor advance),
  but this is an implementation detail and not a wire contract. Clients
  must not assume caching semantics.
- **Pushed snapshots**: `session/hydrate` is request-driven only. A
  notification variant ("server-initiated hydrate") is not part of this
  UPCR; if needed, it requires a separate accepted UPCR.
