# Octos UI Protocol Change Request: `turn/state/get` Command

## Header

- Request id: `UPCR-2026-011`
- Title: Add `turn/state/get` RPC command for deterministic turn lifecycle introspection
- Author: 5-day structural plan, Day 1 (coding-green)
- Date: 2026-04-30
- Target protocol: `octos-ui/v1alpha1`
- Status: proposed
- Related issues: `#738`, `#740` (thread-binding bug chain — wire-side misroute observed when clients fall back to "current sticky thread" instead of the originating turn)
- Related plan: `/tmp/octos-architecture-FINAL.md` § Day 1 (UPCR-2026-011)
- Sibling UPCRs: `UPCR-2026-009` (session/hydrate), `UPCR-2026-010` (thread/graph/get), `UPCR-2026-012` (message/persisted)

## Summary

This change request adds one additive JSON-RPC command method, `turn/state/get`, to the AppUi protocol. The command returns the authoritative lifecycle state of one turn: `active | interrupting | completed | errored | interrupted | unknown`, plus optional `started_at`, `completed_at`, `thread_id`, and the list of `committed_seqs` for the turn's persisted messages.

The server already maintains this information in the active-turn registry at `crates/octos-cli/src/api/ui_protocol.rs:2030-2135` (the `ActiveConnectionTurns` map keyed by `(SessionKey, TurnId)`). Today the registry is internal — clients must INFER turn state from notification ordering (`turn/started → message/delta* → turn/completed | turn/error`). When a client misses or interleaves notifications during reconnect, this inference fails.

The change is strictly additive: no existing method, notification, payload, enum variant, capability bit, or protocol identifier is modified.

## Motivation

The 5-day structural plan traces the live-wire side of the thread-binding bug chain (`#738`, `#740`, sticky-map drift in `crates/octos-bus/src/api_channel.rs:97-103`) to a single root cause: when a late-arriving server frame (e.g., `edit_message`, `send_raw_sse`) arrives without a typed thread_id, the channel falls back to "the most-recent sticky thread for this chat_id" — which has been rotated forward by intervening user messages.

The fundamental question the channel cannot answer today is: **"is the originating turn for this frame still active, and what thread_id did it own?"**. A server can answer this trivially — the active-turn registry already exists. But the client cannot ask. Two consequences:

- **#740**: the SPA cannot verify whether a streaming token belongs to the turn it remembered or the latest turn the server thinks is live. The reducer is forced to use ambient sticky state.
- **PR #742**: the persistence-side fix pre-stamped `thread_id`, but the wire-side counterpart (PR-F in the structural plan) needs a typed-bound channel API where the originating turn's thread_id is known. Clients then need a way to verify, post-emission, that the stamp matches the server's record.

A `turn/state/get` command lets:

- Web SPA: confirm a turn is still alive before reading its bubble. Detect stale optimistic state on resume.
- TUI: surface "interrupting" / "errored" turn state in the activity pane without waiting for the next notification.
- Harness fixtures (PR H): assert turn lifecycle deterministically without relying on event-ordering heuristics.
- Debug tooling: query state of a turn from a CLI or browser devtools.

## Change Type

Additive method. One new JSON-RPC command method on the existing AppUi v1alpha1 protocol. No new notifications. No existing method, notification, required field, enum variant, or capability flag is modified. One additive feature flag (`state.turn_state_get.v1`) is added so clients can negotiate availability.

## Wire Contract

Affected wire surface — strictly additive:

- Capability payload: `UiProtocolCapabilities` (new feature flag entry, full-protocol method-set entry)
- Capability feature registry: `state.turn_state_get.v1`
- Command method: `turn/state/get` (new)
- Command params: `TurnStateGetParams` (new)
- Command result: `TurnStateGetResult` (new)

### `turn/state/get`

Purpose:

- Return the canonical lifecycle state of one turn as observed by the server's active-turn registry.

Params:

```json
{
  "session_id": "local:demo",
  "turn_id": "01900000-0000-7000-8000-000000000010"
}
```

- `session_id` (required): canonical session identifier.
- `turn_id` (required): the turn whose state to query.

Result:

```json
{
  "session_id": "local:demo",
  "turn_id": "01900000-0000-7000-8000-000000000010",
  "state": "active",
  "started_at": "2026-05-01T18:30:00Z",
  "completed_at": null,
  "thread_id": "thread-1",
  "committed_seqs": [17, 18, 19]
}
```

Required fields:

- `session_id` (echoed)
- `turn_id` (echoed)
- `state`: open string registry, snake_case, with initial values:
  - `active` — turn is in flight
  - `interrupting` — interrupt requested, server stopping the turn
  - `completed` — terminal: turn finished normally
  - `errored` — terminal: turn finished with an error
  - `interrupted` — terminal: turn was interrupted
  - `unknown` — server has no record of this turn (already evicted, never existed, or wrong session)

Optional fields:

- `started_at` (RFC 3339): when the turn first transitioned to `active`. Absent for `unknown` and may be absent for legacy turns that pre-date the field.
- `completed_at` (RFC 3339): when the turn reached a terminal state. Present iff `state` is one of `completed | errored | interrupted`.
- `thread_id` (string): the thread the turn is bound to. Always present for non-`unknown` states once the typed-binding refactor (PR-F in the structural plan) lands. May be absent on legacy active turns from older daemon versions.
- `committed_seqs` (`u64[]`): the persisted-message seqs owned by this turn (user, assistant, tool, recovery). Allows clients to cross-check `message/persisted` notifications against the canonical set. Empty for `unknown` and for turns that have started but not yet committed any messages.

Future state values must be registered via UPCR.

### Errors

Follow the v1 error taxonomy (see § 10 of the spec):

- `unknown_session` if the session identifier is unknown to the server.
- `invalid_params` (with `data.kind = "invalid_turn_id"`) if `turn_id` is empty or malformed.
- `runtime_unavailable` (with `data.kind = "runtime_unavailable"`) if the server has no active-turn registry wired (e.g., during early startup or in certain test harness configurations).

A `turn/state/get` request for a known session but unknown turn returns `state: "unknown"` rather than an error, matching the precedent set by `task/list` (UPCR-2026-005) returning empty `tasks` for unknown sessions.

## Capability Negotiation

Capability feature: `state.turn_state_get.v1`

- Advertised through optional `supported_features` in `UiProtocolCapabilities` (`crates/octos-core/src/ui_protocol.rs::UiProtocolCapabilities`).
- Clients request it through `X-Octos-Ui-Features` using comma or space-separated feature tokens, matching the precedent from UPCR-2026-001 (typed approvals) and UPCR-2026-007 (session capabilities).
- If the client did not request the feature, the server MUST NOT include `turn/state/get` in `supported_methods` and MUST return JSON-RPC `-32601 Method not found` if it is invoked.
- The capability schema version is `1`.

The feature MUST always be included in the server's known feature registry once shipped. A server that returns `supported_features` containing `state.turn_state_get.v1` MUST handle the method.

## Compatibility

- All existing clients (octos-tui, octos-app, web SPA) continue to function unchanged. The method is opt-in via `X-Octos-Ui-Features`.
- Legacy daemon versions that do not implement `turn/state/get` will not advertise the feature in `supported_features`, and clients will not invoke it.
- The active-turn registry already exists and is populated for every turn started after `turn/start`. No daemon-side state migration is required.
- Telegram, Discord, Email, Matrix, and other linear channels: unaffected. They do not expose UI Protocol; their state is observable only via REST and channel-native message ids.

## Testing Strategy

### Server-side

- Unit tests at `crates/octos-cli/src/api/ui_protocol.rs::tests`:
  - `turn_state_get_returns_active_for_in_flight_turn`
  - `turn_state_get_returns_completed_after_turn_completed_event`
  - `turn_state_get_returns_errored_after_turn_error_event`
  - `turn_state_get_returns_interrupted_after_turn_interrupt_command`
  - `turn_state_get_returns_unknown_for_evicted_turn`
  - `turn_state_get_rejects_unknown_session`
  - `turn_state_get_rejects_empty_turn_id`
  - `turn_state_get_negotiation_excluded_when_feature_not_requested`
  - `turn_state_get_negotiation_included_when_feature_requested`
- Integration test at `crates/octos-cli/tests/m9_protocol_turn_state_get.rs`:
  - Drive a real session through `turn/start → message/delta → turn/completed`; assert `turn/state/get` returns each state at the expected boundary.

### Client-side

- TUI fixture at `octos-tui/tests/...`: render activity pane after invoking `turn/state/get` against a paused turn. Assert "interrupting" surfaces.
- Layer 1 SPA fixture (PR H): the `slow-then-fast-interleave.fixture.ts` and `m89-recovery-turn.fixture.ts` fixtures should call `turn/state/get` between events and assert the returned state matches the expected lifecycle.

### Wire contract

- Golden test in `crates/octos-core/src/ui_protocol.rs` (the literal protocol contract gate):
  - `golden_turn_state_get_params_serde`
  - `golden_turn_state_get_result_serde`
  - `golden_capabilities_includes_turn_state_get_v1_when_negotiated`

## Rollout

- **Day 1 (this UPCR)**: drafted, reviewed, accepted before any code lands.
- **Day 2 (PR G in the structural plan)**: server handler implemented under capability gate. octos-tui adopts in the same PR. Web SPA does not adopt until PR J (Day 4).
- **Pre-release validation**: golden tests pass; `cargo test -p octos-core --features api` covers the capability-negotiation behavior.

## Risks

| Risk | Severity | Mitigation |
|---|---|---|
| Active-turn registry is in-memory only and evicted on daemon restart | Low | The `unknown` state is the documented response. Clients should not depend on `turn/state/get` for terminal-state introspection of a turn older than the daemon's lifetime. |
| `committed_seqs` could grow unboundedly for very long turns | Low | The registry only retains active turns; once terminal, the turn is evicted from the registry within seconds. The `committed_seqs` for a still-active turn is bounded by per-turn message count (typically <100). |
| Future enum variants on `state` may break exhaustive-match clients | Low | The v1alpha1 contract already mandates clients treat unknown enum variants as forward-compatible (§4 of the spec). Capability flag scoping prevents unrequested variants from arriving. |
| Race: client invokes `turn/state/get` between the actor emitting `turn/completed` and the registry transitioning to `completed` | Low | The registry transitions BEFORE the notification is emitted; the race is server-side only and resolved by the actor's strict-ordering invariant. Add a regression test. |
| Method-set disagreement with `task/list` (which returns empty for unknown sessions) | Low | Documented in the Errors section. The two methods consistently return a "not found" sentinel rather than throwing, matching v1's idiom. |

## Open Questions

- Should `committed_seqs` be paginated? **Decision**: no for v1. Active turns rarely have >100 commits; if usage grows, add a follow-up UPCR with `cursor` semantics.
- Should there be a sibling `turn/list` for enumerating all active turns in a session? **Decision**: deferred. The use case is debugger/operator workflows, not the bug class this UPCR closes. Track as a separate `UPCR-2026-XXX` candidate.
- Should the result include the originating `client_message_id`? **Decision**: yes, but as a follow-up additive field. v1 of this UPCR returns `thread_id` only; v2 can extend with `client_message_id` once the typed-identity work in PR A lands and the canonical relationship `client_message_id == thread_id` is enforced server-side.

## Acceptance Criteria

- [ ] Reviewer sign-off on the wire contract.
- [ ] Reviewer sign-off on the capability-flag name.
- [ ] Reviewer sign-off on the `state` enum value list.
- [ ] No conflicts with the existing AppUi v1alpha1 method set.
- [ ] No conflicts with sibling UPCRs (-009, -010, -012).
- [ ] Status flipped to `accepted` before implementation lands in `crates/octos-cli/src/api/ui_protocol.rs`.
