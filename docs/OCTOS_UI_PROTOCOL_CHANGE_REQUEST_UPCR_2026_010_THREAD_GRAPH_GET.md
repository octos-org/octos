# Octos UI Protocol Change Request: `thread/graph/get` Command

## Header

- Request id: `UPCR-2026-010`
- Title: Add `thread/graph/get` RPC command exposing the inferred thread graph
- Author: 5-day structural plan, Day 1 (coding-green)
- Date: 2026-04-30
- Target protocol: `octos-ui/v1alpha1`
- Status: proposed
- Related issues: `#742` (thread-binding pre-stamp at persist time — exposed
  the gap that the inferred grouping is invisible to clients)
- Related plan: `/tmp/octos-architecture-FINAL.md` § Day 1 (UPCR-2026-010)

## Summary

This change request adds one additive JSON-RPC command method,
`thread/graph/get`, to the AppUi protocol so clients can fetch the canonical
thread graph for a session — the partition of messages into render-grouping
threads, plus any orphan rows the grouping rule could not assign. Today the
grouping is **inferred at runtime** by `Session::threads()` in
`crates/octos-bus/src/session.rs` (target call site in the structural plan:
`session.rs:469-514` after PR A's typed-identity refactor). The grouping
function exists; it is just not on the wire. Clients must reconstruct it
from message-ordering heuristics, which is exactly what produced the
mis-binding bugs `#738` / `#740` / `#742`.

The change is strictly additive: no existing method, notification, payload,
enum variant, capability bit, or protocol identifier is modified.

## Motivation

In the v1 chat model:

- `client_message_id` is the client's idempotency token (web SPA mints it).
- `turn_id` is the server's per-turn protocol identity.
- `thread_id` is the **render grouping** — which DOM bubble a row should
  appear under.

The structural plan identifies these as three distinct types with three
distinct sources of truth (`/tmp/octos-architecture-FINAL.md` § Three IDs).
The third — `thread_id` — has the weakest contract today: it is currently
optional, derived, and only exists in-memory as the output of
`Session::threads()`. When the grouping function decides a row belongs to
thread X, that decision is invisible to the client. The client renders the
row under whichever thread its own heuristic says — and when the two
heuristics disagree, a bug ships.

Codifying the grouping on the wire as `thread/graph/get` would have surfaced
PR `#742`'s gap immediately. Specifically:

- A persisted assistant row mis-routed under Q3 instead of Q1 would either
  show up under the wrong `thread_id` in the response (visible in test
  fixtures, easily asserted) or land in `orphans` (loud signal). Either way
  the bug is observable, not silent.
- The Layer 1 reducer fixtures from Day 3 of the structural plan (`PR H`)
  can use the response shape directly as the assertion target — no
  client-side reconstruction.

`thread/graph/get` is split out from `session/hydrate` (`UPCR-2026-009`)
because:

1. The thread graph is a small, stable projection that clients may want to
   refetch frequently (e.g. after every turn completes) without paying the
   full hydrate cost.
2. Test fixtures and debugger tools want to inspect the graph in isolation.
3. The capability gate is independent: a client may want the thread graph
   without committing to the bundled hydrate semantics.

## Change Type

Additive method.

One new JSON-RPC command method on the existing AppUi v1alpha1 protocol. No
new notifications. No existing method, notification, required field, enum
variant, or capability flag is modified. One additive feature flag
(`state.thread_graph.v1`) is added so clients can negotiate availability.

## Wire Contract

Affected wire surface — strictly additive:

- Capability payload: `UiProtocolCapabilities` (new feature flag entry,
  full-protocol method-set entry)
- Capability feature registry: `state.thread_graph.v1`
- Command method: `thread/graph/get` (new)
- Command params: `ThreadGraphGetParams` (new)
- Command result: `ThreadGraphGetResult` (new)
- Wire payload: `Thread` (new — also reused by `UPCR-2026-009`)

No existing command method, notification, params, results, or enum variants
are modified by this UPCR.

### `thread/graph/get`

Purpose:

- Return the current thread graph for one session: the set of threads with
  their member message seqs, plus any rows the grouping rule could not
  assign. Primary consumers: web SPA reload reconciliation, TUI thread
  navigation, debugger inspection, Layer 1 reducer fixtures.

Params:

```json
{
  "session_id": "local:demo",
  "at": { "stream": "session", "seq": 142 }
}
```

- `session_id` (required): canonical session identifier.
- `at` (optional `UiCursor`): when present, return the thread graph as it
  stood at the given cursor (point-in-time projection). Absent = current
  head. Useful for fixtures that want a deterministic snapshot.

Result:

```json
{
  "session_id": "local:demo",
  "cursor": { "stream": "session", "seq": 142 },
  "threads": [
    {
      "thread_id": "thread-1",
      "root_seq": 17,
      "root_client_message_id": "01900000-0000-7000-8000-000000000001",
      "turn_id": "01900000-0000-7000-8000-000000000010",
      "message_seqs": [17, 18, 19],
      "status": "completed"
    },
    {
      "thread_id": "thread-2",
      "root_seq": 22,
      "root_client_message_id": "01900000-0000-7000-8000-000000000002",
      "turn_id": "01900000-0000-7000-8000-000000000011",
      "message_seqs": [22, 23],
      "status": "active"
    }
  ],
  "orphans": [42]
}
```

Field descriptions:

- `session_id` (required): echoes the request.
- `cursor` (required `UiCursor`): the cursor at which the graph was assembled.
  When the request specified `at`, this echoes that value; otherwise it is
  the current ledger head for the session.
- `threads` (required `Thread[]`, may be empty): the partition of message
  seqs into threads. Each entry:
  - `thread_id` (required `string`): canonical thread identifier — stable
    across reloads, mints from the server's grouping rule.
  - `root_seq` (required `u64`): the sequence number of the user message
    that started the thread.
  - `root_client_message_id` (optional `string`): the client-minted
    idempotency token of the root user message. Absent for legacy rows that
    pre-date `client_message_id` (loaded via `synthesize_thread_ids_for_legacy`,
    see `/tmp/octos-architecture-FINAL.md` § What does NOT change).
  - `turn_id` (optional `string`): the canonical turn id for this thread,
    when the originating turn is known to the server. Absent for legacy
    threads or for rows persisted before `turn_id` was on the type.
  - `message_seqs` (required `u64[]`): the seqs of all messages bound to
    this thread, in commit order. Always non-empty (at minimum carries the
    root user seq).
  - `status` (required `string`): the thread's lifecycle state. Initial
    string registry:
    - `active` — the originating turn is still running.
    - `completed` — the originating turn reached a terminal `completed` state.
    - `errored` — the originating turn reached a terminal `errored` state.
    - `interrupted` — the originating turn was interrupted.
    - `unknown` — the server has no live record of the originating turn
      (e.g. legacy load, server restart).
- `orphans` (required `u64[]`, may be empty): seqs of persisted messages
  the grouping rule could not bind to any thread. Examples: a tool result
  whose originating turn the server has lost track of (legacy data); a
  recovery row from a crash whose turn never re-emitted. **Orphans should
  be rare in healthy operation; a client observing non-empty `orphans` in
  steady state should log a metric.**

A `thread/graph/get` request for a known but empty session returns success
with `threads: []`, `orphans: []`, and the current head cursor. Empty is
not an error.

### Why the result mirrors `Session::threads()`

The result fields are a 1:1 lift of the existing internal `Thread` struct
that `Session::threads()` constructs (target location after PR A's
typed-identity refactor: `crates/octos-bus/src/session.rs:469-514`). The
grouping logic does not change — only its visibility on the wire.

PR A from the structural plan promotes `thread_id` from `Option<String>` to
a typed `ThreadId` newtype. This UPCR's wire shape uses the JSON
serialization of that newtype (a plain `string`), so the wire format is
stable across the type-system upgrade.

## Error Model

The new command returns errors from the existing v1 taxonomy:

- `unknown_session` — server has no session with the given `session_id`, or
  the session is scoped to a different connection profile than the request.
- `cursor_out_of_range` — the supplied `at` cursor addresses a position
  beyond the current ledger head.
- `cursor_invalid` — the supplied `at` cursor is malformed or wrong-stream.
- `runtime_unavailable` — server has no chat-state projection wired for
  this deployment. Returned with `data.kind = "runtime_unavailable"`.

No new error categories are introduced.

## Compatibility

- Old clients that never request `state.thread_graph.v1` and never send
  `thread/graph/get` are unaffected.
- Old servers that have not implemented this method reject incoming
  requests with the existing `method_not_supported` error.
- Clients that exhaustively match on `UiCommand` or `UiResultKind` and have
  not been recompiled against the new enum variants will fail to deserialize
  a message carrying the new method. Standard forward-compatibility
  behaviour for any added method (spec § 4.1).
- The `Thread` payload shape is shared with `SessionHydrateResult.threads`
  in `UPCR-2026-009`. Both UPCRs reference the same JSON shape; a future
  schema change to `Thread` requires extending both UPCRs simultaneously,
  or a strictly additive change governed by spec § 4 (additive optional
  fields allowed within a version).
- `thread_id` is currently optional in the wire model (`message/delta`,
  persisted messages). This UPCR does **not** change that — it only adds
  a separate, dedicated query for the graph. The `thread_id` evolution
  on persisted messages is governed by PR A of the structural plan and is
  out of scope here.
- No new protocol identifier is required because the change is additive.

## Capability Negotiation

New feature flag:

- `state.thread_graph.v1`

Servers advertise it through `UiProtocolCapabilities.supported_features`
when the thread-graph projection is available. The flag is included in
`UiProtocolCapabilities::full_protocol()` and in the first server slice's
supported method set. Clients that want to depend on `thread/graph/get`
should request the feature through `X-Octos-Ui-Features` and read back
the negotiated set from `SessionOpened.capabilities` (per `UPCR-2026-007`).

If a client sends `thread/graph/get` to a server that does not advertise
the feature, the server returns `method_not_supported`.

## Tests

- `crates/octos-core/src/ui_protocol.rs`:
  - `thread_graph_get_command_round_trips_through_json_rpc` — round-trips
    the request including the optional `at` cursor.
  - `thread_graph_get_result_round_trips_with_orphans` — golden:
    response with non-empty `orphans` round-trips through serde.
  - `thread_graph_get_result_round_trips_with_legacy_thread` — golden:
    a thread with `root_client_message_id: None` and `turn_id: None`
    round-trips (legacy-load case).
  - `thread_status_string_registry_is_stable` — golden: the five known
    `status` literals (`active`, `completed`, `errored`, `interrupted`,
    `unknown`) serialize as snake_case and round-trip.
  - `typed_rpc_results_map_from_methods_and_round_trip` — extended with
    `ThreadGraphGetResult` golden coverage.
  - `full_protocol_capabilities_advertise_state_thread_graph` — asserts
    the new feature flag and method are advertised in `full_protocol()`.
- `crates/octos-cli/src/api/ui_protocol.rs`:
  - `appui_thread_graph_get_returns_session_threads` — verifies the
    handler returns the partition produced by `Session::threads()`.
  - `appui_thread_graph_get_reports_orphans_for_unbound_messages` —
    seeds a session with a row whose `turn_id` cannot be resolved;
    verifies it lands in `orphans`.
  - `appui_thread_graph_get_honours_at_cursor` — verifies a point-in-time
    snapshot at an earlier cursor excludes later threads.
  - Routing tests extended with `thread/graph/get` requests.
- `e2e/tests/`:
  - `m9-protocol-thread-graph.spec.ts` — end-to-end: drive three
    interleaved turns, call `thread/graph/get`, assert the partition
    matches the message stream observed via notifications.

## Rollout Plan

1. Land the protocol constants, params/result types, command/result enums,
   `Thread` payload shape, capability flag, and golden tests in `octos-core`.
2. Land the server handler `handle_thread_graph_get` in `octos-cli`,
   delegating to `Session::threads()` for the underlying partition.
   Routing tests added.
3. Update `api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md` § 4.1, § 6, and § 7
   to reference `UPCR-2026-010`.
4. Layer 1 reducer fixtures (Day 3 PR H) consume `thread/graph/get`'s shape
   as the assertion target.
5. Web SPA migration (Day 4 PR J) calls `thread/graph/get` after every
   `turn/completed` notification to reconcile its local thread state.

No client renegotiation is required for clients that do not consume the new
method.

## Risks

- **Cost on long sessions**: a session with thousands of messages produces
  a large `message_seqs` payload. Mitigation: clients can filter to a
  cursor range via `at`, or use `session/hydrate`'s `after` for incremental
  fetches. Per-thread pagination is out of scope; if it proves necessary,
  a follow-up UPCR can add it.
- **Grouping algorithm churn**: any change to the `Session::threads()`
  partition rule changes the wire output. Mitigation: the grouping rule
  is governed by spec § 4 as a semantic surface; behavioural changes
  require their own UPCR. The wire shape stays additive across rule
  changes as long as the response fields don't change meaning.
- **Orphan steady-state**: if `orphans` is consistently non-empty in
  production, that signals an upstream binding bug (the very class
  `#738` / `#740` / `#742` was about). This is the intent — observable,
  alertable. Operators should add a metric on `orphans.len() > 0`.

## Decision

Proposed by: 5-day structural plan, Day 1.

Decision notes: Pending review. Lifts the existing in-memory thread
partition produced by `Session::threads()` onto the wire so clients no
longer reconstruct grouping from message-ordering heuristics. Would have
made `#742`'s mis-binding observable in test fixtures rather than only
in soak. Bundled with `UPCR-2026-009` / `UPCR-2026-011` / `UPCR-2026-012`
as the four state-control primitives the structural plan identifies as
missing from v1.

### Out of scope

- **Per-thread pagination** (limit / offset semantics on `message_seqs`):
  deferred. Snapshot via `at` is the only point-in-time control in this
  UPCR.
- **Push notifications on graph changes**: this UPCR is request-driven
  only. A `thread/graph/changed` notification variant is not part of
  this UPCR; clients should re-fetch after `turn/completed` until / unless
  a separate UPCR adds push semantics.
- **Thread mutation RPCs** (rename, merge, fork): out of scope. This
  UPCR is read-only.
