# Octos UI Protocol Change Request: `message/persisted` Notification

## Header

- Request id: `UPCR-2026-012`
- Title: Add `message/persisted` notification for durable message-row commit confirmation
- Author: 5-day structural plan, Day 1 (coding-green)
- Date: 2026-04-30
- Target protocol: `octos-ui/v1alpha1`
- Status: proposed
- Related issues: `#738`, `#740`, `#742` (thread-binding bug chain — ack/result divergence and reload misbinding under spawn_only flows)
- Related plan: `/tmp/octos-architecture-FINAL.md` § Day 1 (UPCR-2026-012)
- Sibling UPCRs: `UPCR-2026-009` (session/hydrate), `UPCR-2026-010` (thread/graph/get), `UPCR-2026-011` (turn/state/get)

## Summary

This change request adds one additive JSON-RPC notification method, `message/persisted`, to the AppUi protocol. The notification fires once per durable commit of a message row to the session ledger, carrying the canonical `(turn_id, thread_id, seq)` tuple plus the message's identity and durability cursor.

Today the closest signal a client gets is `TurnCompletedEvent`, which carries an optional `cursor` but does NOT confirm which message rows committed under that turn (`crates/octos-core/src/ui_protocol.rs:2455-2461`). Spawn_only flows compound this gap: an ack message ("background work started…") is emitted on the wire, but the eventual result message — possibly arriving minutes later under a recovery path — has no synchronous notification confirming it landed in the ledger.

The change is strictly additive: no existing method, notification, payload, enum variant, capability bit, or protocol identifier is modified.

## Motivation

The 5-day structural plan traces three failure modes to the absence of a durable-commit confirmation event:

1. **Ack/result divergence** (`#738`): `run_pipeline` emits a spawn-ack via `message/delta` + `turn/completed` immediately. The actual result lands minutes later via `synthetic_recovery_inbound` at `crates/octos-cli/src/session_actor.rs:5806-5905`. Today the SPA has no event signaling the durable commit of the result row — only the optional refresh of `cursor` via the next `turn/started`.

2. **Reload misbinding** (`#740`): under rapid-fire interleave, the SPA optimistically renders message bubbles before durable commit. When the WebSocket reconnects (or the user reloads), the reducer cannot reconcile its optimistic bubbles against the server's authoritative view because no event confirms which seq the server actually wrote.

3. **Optimistic-bubble correction**: web SPA's `client_message_id` → `seq` correlation today depends on the `session_result` event payload, which only fires at turn completion. Tools running mid-turn (file delivery, sub-agent output) commit rows that the SPA cannot correlate without a backfill round-trip via REST `GET /api/sessions/:id/messages`.

A `message/persisted` notification — fired once per durable commit, regardless of source path (user input, assistant turn, tool output, background spawn, recovery) — closes all three. It also gives PR H's Layer 1 fixture infrastructure a deterministic event to assert on, replacing the brittle "wait for turn/completed and then read messages" pattern.

## Change Type

Additive notification. One new JSON-RPC notification on the existing AppUi v1alpha1 protocol. No new commands. No existing notification, command, required field, enum variant, or capability flag is modified. One additive feature flag (`event.message_persisted.v1`) is added so clients can negotiate availability.

## Wire Contract

Affected wire surface — strictly additive:

- Capability payload: `UiProtocolCapabilities` (new feature flag entry, full-protocol notification-set entry)
- Capability feature registry: `event.message_persisted.v1`
- Notification method: `message/persisted` (new)
- Notification params: `MessagePersistedEvent` (new)

No existing notification, command, params, results, or enum variants are modified by this UPCR.

### `message/persisted`

Purpose:

- Carry a single, durably-committed message row from the session ledger to subscribed clients. One notification per row, fired AFTER the row is fsynced to the JSONL ledger by `Session::add_message_with_seq` (`crates/octos-bus/src/session.rs:1850-1903`). Idempotent on cursor: the same seq is never emitted twice for the same session.

Notification params:

```json
{
  "session_id": "local:demo",
  "turn_id": "01900000-0000-7000-8000-000000000010",
  "thread_id": "thread-1",
  "seq": 18,
  "role": "assistant",
  "message_id": "01900000-0000-7000-8000-000000000018",
  "client_message_id": "01900000-0000-7000-8000-000000000001",
  "source": "assistant",
  "cursor": { "stream": "session", "seq": 18 },
  "persisted_at": "2026-05-01T18:30:01Z"
}
```

Required fields:

- `session_id`: canonical session identifier.
- `seq`: the authoritative committed sequence number assigned by `add_message_with_seq`. Strictly monotonic per session.
- `role`: open snake_case enum, matching `crates/octos-core::MessageRole`. Initial values: `system | user | assistant | tool`.
- `message_id`: server-assigned UUID for the row. Stable across replays.
- `cursor`: durable cursor pointing at this commit. Clients can use this as their `after` value for subsequent `session/hydrate` and `session/open` calls.
- `source`: open snake_case enum identifying the WRITE PATH that committed this row. Initial values:
  - `user` — direct API ingress (web POST /api/chat, `turn/start`, telegram message, etc.)
  - `assistant` — primary turn assistant output
  - `tool` — tool invocation result (sync tool, attached to a turn)
  - `background` — spawn_only result row (commits after the parent turn's `turn/completed`)
  - `recovery` — synthetic recovery turn row (M8.9 path)
- `persisted_at` (RFC 3339): wall-clock time the row was committed.

Optional fields:

- `turn_id`: the originating turn. Always present once the typed-binding refactor (PR-F in the structural plan) lands. Absent on legacy rows that pre-date the field.
- `thread_id`: the thread grouping. Same enforcement story as `turn_id`.
- `client_message_id`: present for `source = user` rows where the client supplied a cmid; present on `assistant`/`tool`/`background`/`recovery` rows ONCE the typed-identity work in PR A propagates the cmid into derived rows. Absent on legacy rows.

Future `source` values must be registered via UPCR.

### Ordering and Cursor Semantics

`message/persisted` notifications are emitted in **strict commit order per session**. Two consequences:

- A client that consumes `message/persisted` and tracks the latest seen `cursor` has an authoritative, replay-safe view of the session's message log. This is the cursor-based equivalent of the JSONL ledger.
- A client that drops the WebSocket and reconnects with `session/open { after: <last cursor> }` MUST receive `message/persisted` for every seq strictly greater than `after`, in order, before any new live notifications. Implementations MAY batch notifications during replay to bound bandwidth; ordering must be preserved.

Relationship with other notifications:

- `message/delta` carries WIRE-LEVEL incremental tokens (ephemeral). Clients use it for live streaming UX. No durability guarantee.
- `message/persisted` carries DURABLE row commits (post-fsync). Clients use it for state truth.
- `turn/completed` continues to fire once per terminal turn; `message/persisted` fires once per row, possibly multiple times per turn (e.g., assistant + tool calls).

These are NOT redundant: `message/delta` is the streaming UX; `message/persisted` is the durable-state UX. `turn/completed` is turn-lifecycle; `message/persisted` is row-lifecycle.

### Errors

Notifications cannot return errors per JSON-RPC. The server MUST NOT emit `message/persisted` for a row that did not commit. If a commit fails (disk full, fsync error), the server emits a `warning` notification (existing surface) and does NOT emit `message/persisted` for that row.

## Capability Negotiation

Capability feature: `event.message_persisted.v1`

- Advertised through optional `supported_features` in `UiProtocolCapabilities`.
- Clients request it through `X-Octos-Ui-Features` using comma or space-separated feature tokens.
- Servers MUST NOT emit `message/persisted` to a connection that did not negotiate the feature. Pre-existing connections (TUI, octos-app) continue to receive only the events they negotiated.
- The capability schema version is `1`.

## Compatibility

- All existing clients continue to function unchanged. The notification is opt-in via `X-Octos-Ui-Features`.
- Legacy daemon versions that do not implement `message/persisted` will not advertise the feature, and clients will not expect it.
- Server implementation requires emitting the notification from the `add_message_with_seq` post-commit hook. The hook point already exists for the durable-ledger write (`crates/octos-bus/src/session.rs:1872-1894`); this UPCR adds one notification dispatch alongside it.
- Cross-channel: telegram/discord/etc. do not subscribe to UI Protocol notifications. They are unaffected.
- The notification IS emitted for `source = user` rows, which means the SPA can use it to confirm its own POST /api/chat persisted before rendering the optimistic bubble as committed. (This is the missing piece in today's optimistic-bubble flow.)

## Testing Strategy

### Server-side

- Unit tests at `crates/octos-bus/src/session.rs::tests`:
  - `message_persisted_emitted_after_user_commit`
  - `message_persisted_emitted_after_assistant_commit`
  - `message_persisted_emitted_after_tool_commit`
  - `message_persisted_emitted_for_background_spawn_result`
  - `message_persisted_emitted_for_recovery_turn`
  - `message_persisted_not_emitted_on_commit_failure`
  - `message_persisted_strict_ordering_under_concurrent_writes`
  - `message_persisted_idempotent_seq_never_repeated`
- Integration test at `crates/octos-cli/tests/m9_protocol_message_persisted.rs`:
  - Drive a session through user → assistant → tool → spawn_only → recovery; assert one notification per commit, in order, with correct `(turn_id, thread_id, seq, source)` for each.

### Client-side

- TUI fixture at `octos-tui/tests/...`: subscribe to `message/persisted`, assert the message-list view stays consistent with `session/hydrate` after disconnect/reconnect.
- Layer 1 SPA fixture (PR H): every fixture asserts that `message/persisted` events arrive in the expected order with the expected `(turn_id, thread_id, seq)` tuples. The `rapid-fire-five-fast` fixture in particular asserts no row is mis-routed to a sibling thread.

### Wire contract

- Golden test in `crates/octos-core/src/ui_protocol.rs`:
  - `golden_message_persisted_event_serde`
  - `golden_message_persisted_strict_ordering`
  - `golden_capabilities_includes_message_persisted_v1_when_negotiated`

## Rollout

- **Day 1 (this UPCR)**: drafted, reviewed, accepted before any code lands.
- **Day 2 (PR G in the structural plan)**: server emitter implemented under capability gate. The hook lives in `crates/octos-bus/src/session.rs::add_message_with_seq` post-commit, so it fires for ALL write paths (user, assistant, tool, spawn-only, recovery) without per-call-site changes.
- **Day 3 (PR H Layer 1 fixtures)**: fixtures consume the notification as the deterministic durability signal.
- **Day 4 (PR J web SPA migration)**: SPA subscribes; uses `message/persisted` to correlate optimistic bubbles to durable seqs without backfill round-trip.

## Risks

| Risk | Severity | Mitigation |
|---|---|---|
| Notification volume on busy sessions | Medium | Each row triggers exactly one notification. For sessions with high tool-call density, this could approach 10/s for short windows. Wire compression (per-frame brotli over WS) handles this; payload is ~250 bytes per notification. Add a benchmark gate at 100/s sustained rate. |
| Replay storm on reconnect after long offline window | Medium | The capability gate scopes replay to clients that opted in. Clients that need only live state can skip the feature. For clients that need replay, batching during replay (existing pattern, see UPCR-2026-002 panes replay) limits bandwidth. |
| Backwards-compat with legacy `Message.thread_id: None` rows | Low | The notification's `thread_id` field is optional (matching `Message`). Legacy rows emit notifications with absent/synthesized thread_id, identical to today's reload behavior. |
| Race: emit before client subscribed | Low | The capability flag is evaluated at session/open time. Notifications for rows committed before session/open arrive only via cursor-based replay through `session/hydrate` (UPCR-2026-009), which carries the same data. |
| Cross-cancel semantics | Low | A cancelled turn's already-committed rows still emit `message/persisted`. The terminal `turn/error` or `turn/interrupted` notification carries the lifecycle truth; `message/persisted` carries the row truth. The two are independent. |

## Open Questions

- Should `message/persisted` carry the row's full content (`text`)? **Decision**: no for v1. Including the content makes the notification large (kilobytes for assistant messages) and duplicates `message/delta`. Clients that need content fetch via `session/hydrate` (UPCR-2026-009).
- Should there be a separate `tool/persisted` notification for tool rows? **Decision**: no. The `source = tool` discriminator on the unified notification is sufficient.
- Should the notification include the row's `client_message_id` for assistant/tool rows that inherit it? **Decision**: yes once PR A's typed-identity work lands; the field is defined as optional in v1 and becomes always-present in v2 once the inheritance is enforced server-side.

## Acceptance Criteria

- [ ] Reviewer sign-off on the wire contract (param shape, source enum values).
- [ ] Reviewer sign-off on the capability-flag name and version.
- [ ] Reviewer sign-off on ordering guarantees (strict per-session monotonic seq).
- [ ] No conflicts with sibling UPCRs (-009 hydrate, -010 thread-graph, -011 turn-state).
- [ ] No regression on the existing `turn/completed` cursor-update behavior — the new notification is supplementary, not a replacement.
- [ ] Status flipped to `accepted` before implementation lands in `crates/octos-bus/src/session.rs::add_message_with_seq` post-commit hook.
