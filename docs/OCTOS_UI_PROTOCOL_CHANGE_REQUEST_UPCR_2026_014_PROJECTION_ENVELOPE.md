# Octos UI Protocol Change Request: M9-γ Projection Envelope

## Header

- Request id: `UPCR-2026-014`
- Title: Add canonical M9-γ `Envelope` shape (`thread_id`, `seq`,
  `client_message_id?`, `payload`) for deterministic web client projection
- Author: M9-γ-1 worker (coding-green)
- Date: 2026-05-09
- Target protocol: `octos-ui/v1alpha1`
- Status: accepted
- Related issues: `#838` (M9-γ-1 envelope spec)
- Related ADR: [`docs/M9-GAMMA-SERVER-PROJECTION-ADR.md`](./M9-GAMMA-SERVER-PROJECTION-ADR.md)
- Sibling UPCRs: `UPCR-2026-009` (session/hydrate), `UPCR-2026-010`
  (thread/graph/get), `UPCR-2026-011` (turn/state/get), `UPCR-2026-012`
  (message/persisted)

## Summary

This change request adds the canonical M9-γ projection envelope shape to
the AppUi protocol. The web client's deterministic projection consumes
an append-only `Vec<Envelope>` indexed by `(thread_id, seq)` and a pure
projection function `(committed_log) → ChatViewModel` produces the
rendered chat. Identity collapses to `seq` — the only key the projection
cares about. `client_message_id` lives ONLY on `user_message` envelopes
so the optimistic `<GhostBubble>` overlay can match its server
reflection and unmount; the projection itself never consults it.

The eight payload variants are: `user_message` (turn root,
server-mirrored from the client's send), `assistant_delta` (streamed
fragment — APPENDS in `seq` order), `assistant_persisted` (final text
with durable `MessageMeta` — REPLACES the streamed deltas),
`tool_start` / `tool_progress` / `tool_end` (with `complete | error |
skipped | aborted` status and an optional `reason`), `file_attached`,
and `turn_completed` (hard barrier — any later envelope on the same
`thread_id` is DROPPED by the projection and counted in the
`octos_projection_post_completion_drop_total` metric; threads are not
reused).

The change is strictly additive: no existing notification, command,
payload, enum variant, or capability flag is modified. The legacy
`message/delta`, `message/persisted`, `tool/*`, and `turn/completed`
notifications continue to flow on connections that do not negotiate
this feature, until `M9-γ-3` deletes them.

## Motivation

The M9-γ ADR traces the entire "Q1 works, Q2 disappears" /
"phantom-bubble" / "ack-result-divergence" / "reload-misbinding" bug
class to a single root cause: the web client's `ThreadStore` is a
*parallel mutable copy* of server state, with ten reducer entry points
each doing partial reconciliation against a different identity field
(`clientMessageId`, `messageId`, `historySeq`, `intra_thread_seq`,
`threadId`, `responseToClientMessageId`). Under race conditions they
contradict each other and produce phantom rows.

The fix is structural, not transport: collapse identity to `(thread_id,
seq)`, make the projection a pure function, and move optimistic UI to a
visual-only `<GhostBubble>` overlay that never enters the projection.

This UPCR defines the canonical wire envelope the projection consumes.
Server emit support and client projection consumption land in
follow-up M9-γ-N issues; `γ-1` is the spec lock.

## Change Type

Additive notification shape. One new wire-level type union (`Envelope`,
`Payload`) is defined. One additive feature flag
(`projection.envelope.v1`) is added so clients can negotiate
availability. No existing notification, command, params, results, or
enum variants are modified by this UPCR.

## Wire Contract

Affected wire surface — strictly additive:

- Capability payload: `UiProtocolCapabilities.supported_features` (new
  feature flag entry)
- Capability feature registry: `projection.envelope.v1`
- Wire envelope shape: `Envelope` (new, see § 14 of the spec)
- Wire payload tagged union: `Payload` (new, see § 14.2 of the spec)
- Supporting types: `MessageMeta`, `EnvelopeTokenUsage`,
  `EnvelopeToolEndStatus`, `FileRef`

Spec section: § 14 "M9-γ Envelope" of
`api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md`.

### `Envelope`

```json
{
  "thread_id": "thread-1",
  "seq": 18,
  "client_message_id": "01900000-0000-7000-8000-000000000001",
  "payload": { "type": "...", "data": { ... } }
}
```

Required fields:

- `thread_id` (`string`) — Multi-turn cluster identity.
- `seq` (`u64`) — Server-assigned strict total order WITHIN the
  `thread_id`. Strictly monotonic; gaps are an error.
- `payload` (object) — Sealed tagged union (see below).

Optional fields:

- `client_message_id` (`string`) — Populated ONLY on `user_message`
  envelopes. Absent on every other variant. The projection MUST NOT
  consult this field; it exists for the optimistic overlay's
  match-and-unmount.

### `Payload` variants

Wire form: `{ "type": <snake_case>, "data": <variant-specific> }`.
Mirrors Rust `serde(tag = "type", content = "data", rename_all =
"snake_case")`.

- `user_message { text: string, files: FileRef[] }` — Turn root,
  server-mirrored from the client's send. The carrying envelope's
  `client_message_id` is populated here (and ONLY here). `files` is
  omitted on the wire when empty.
- `assistant_delta { text: string }` — One streamed text fragment.
  APPENDS to the live bubble in `seq` order.
- `assistant_persisted { text: string, meta: MessageMeta }` — Final
  text + durable identity. **REPLACES** the accumulated streamed
  deltas for the same `thread_id` (canonical final form). See § 14.3.
- `tool_start { tool_call_id: string, name: string }` — Tool invocation
  begun.
- `tool_progress { tool_call_id: string, message: string }` — Tool
  progress update.
- `tool_end { tool_call_id: string, status: "complete" | "error" |
  "skipped" | "aborted", error?: string, reason?: string }` — Tool
  finished. `error` set iff `status === "error"`. `reason` is
  optional human-readable detail (primary use: `skipped` /
  `aborted`).
- `file_attached { path: string, mime: string, size_bytes: u64 }` —
  File attached to current thread. Embeds the same triple as `FileRef`.
- `turn_completed { token_usage: EnvelopeTokenUsage }` — **Hard
  barrier**; see § 14.6 of spec.

Future variants must be registered via UPCR.

### Turn shape and reconciliation rule

- Every chat turn begins with exactly one `user_message` envelope, and
  the projection's `UserView` is reconstructed from `user_message`
  envelopes alone. `assistant_delta` / `assistant_persisted` cannot
  rebuild the user side of the chat on a refresh-only projection.
- `assistant_delta.text` events APPEND to the live bubble in strict
  `seq` order. When an `assistant_persisted` envelope arrives for the
  same `thread_id`, its `text` field REPLACES the accumulated
  streamed text — the persisted form is canonical and avoids
  double-rendering the final body.

### Hard barrier semantics (drop-with-metric)

Per the M9-γ ADR and § 14.6 of the spec, the server MUST emit at most
one `turn_completed` envelope per `(thread_id, turn)`. After it:

> Any envelope arriving on the same `thread_id` is DROPPED by the
> projection (and counted in
> `octos_projection_post_completion_drop_total`). Threads are NOT
> reused — a new turn must use a NEW `thread_id`.

The drop is silent at the projection layer. Clients do NOT rehydrate
or treat the situation as a desync. The M9-γ-2 projection
([`octos-web` PR #93](https://github.com/octos-org/octos-web/pull/93))
implements the same behaviour and is the canonical reference.

### Identity model

Per § 5.1 of the spec:

- The projection key is `(thread_id, seq)`.
- `client_message_id` is OPTIONAL and ONLY for the optimistic overlay.
- The legacy per-row `message_id` (carried on
  `MessagePersistedEvent.message_id` and on
  `assistant_persisted.meta.message_id`) is **deprecated for
  projection identity**. It is retained for audit/render display so
  legacy `appendCompletionBubble` / `message/persisted` consumers
  continue to work until `M9-γ-3` deletes them.

## Capability Negotiation

Capability feature: `projection.envelope.v1`

- Advertised through optional `supported_features` in
  `UiProtocolCapabilities`.
- Clients request it through `X-Octos-Ui-Features` (comma- or
  space-separated tokens).
- Servers MUST NOT emit canonical `Envelope` notifications to a
  connection that did not negotiate the feature. Pre-existing
  connections (TUI, octos-app legacy) continue to receive only the
  legacy notification surface they negotiated.
- The capability schema version remains `2` — this is additive.

## Compatibility

- All existing clients continue to function unchanged. The envelope is
  opt-in via `X-Octos-Ui-Features`.
- Legacy daemon versions that do not implement canonical envelopes
  will not advertise the feature, and clients will not expect them.
- The deprecation note in § 5.1 of the spec is informational; legacy
  identity fields stay on the wire until `M9-γ-3` retires them
  behind a separate flag flip (see ADR phase plan).
- Cross-channel: telegram/discord/etc. do not subscribe to UI Protocol
  notifications; unaffected.

## Testing Strategy

### Server-side / wire contract

- Golden tests in `crates/octos-core/src/ui_protocol.rs::tests`:
  - `golden_envelope_assistant_delta_round_trips`
  - `golden_envelope_user_message_round_trips`
  - `golden_envelope_user_message_omits_empty_files`
  - `golden_envelope_assistant_delta_omits_client_message_id_on_wire`
  - `golden_envelope_assistant_persisted_round_trips`
  - `golden_envelope_tool_start_progress_end_round_trip`
  - `golden_envelope_tool_end_skipped_and_aborted_round_trip`
  - `golden_envelope_file_attached_round_trips`
  - `golden_envelope_turn_completed_round_trips`
  - `golden_envelope_token_usage_zero_default_round_trips`
  - `golden_envelope_capability_feature_flag_registered`

  These lock the wire shape: `serde_json` round-trip, snake_case
  discriminator presence, optional-field omission on the wire
  (`client_message_id` on non-`user_message` envelopes, `files` when
  empty, `error`, `reason`, zero `token_usage` fields), and the
  closed-union extension of `EnvelopeToolEndStatus` to four variants.

### Client-side

- TS types in `crates/octos-web/src/runtime/ui-protocol-types.ts`
  mirror the Rust enum bit-for-bit. `tsc --noEmit` keeps them honest.
- M9-γ-2 will land property tests for the projection function
  (replay determinism: any `seq`-ordered prefix of a turn's envelopes
  → byte-identical `ChatViewModel`).
- M9-γ-3 fixtures will assert that ingesting an envelope after a
  same-turn `turn_completed` triggers the rehydrate code path (the
  hard-barrier enforcement).

## Rollout

- **γ-1 (this UPCR)**: spec lands; types regenerated for Rust + TS;
  capability flag wired into the known-features registry.
- **γ-2**: pure-function `projection(envelopes) → ChatViewModel` lands
  in `octos-web/src/runtime/projection.ts` behind the
  `chat_projection_v1` flag.
- **γ-3**: `ThreadStore` cutover — the ten reducer entry points
  collapse to one `ingest(envelope)` dispatcher.
- **γ-4**: `<GhostBubble>` overlay component lands; optimistic UI moves
  out of `ThreadStore`.
- **γ-5**: identity collapse — `clientMessageId` /
  `responseToClientMessageId` / `intra_thread_seq` removed from
  projection code.
- **γ-6**: `MessageStore` deletion.
- **γ-7**: server-side cleanup — server emits exactly N+1 envelopes
  per turn (N tool/assistant cycles + 1 `turn_completed`).

Each step is gated behind 7-day soak green per ADR.

## Risks

| Risk | Severity | Mitigation |
|---|---|---|
| Wire size: each envelope is bigger than today's `message/delta` because of the `payload` wrapper | Low | The `payload` wrapper adds ~30 bytes/event vs the legacy flat shape. WS per-frame compression already in place; payload size is negligible at chat bandwidth scales. |
| `client_message_id` leak into projection code | Medium | Spec § 14.1 + ADR explicitly forbid it. The projection's TS type narrows on `payload.type` exclusively; tests in γ-2 will assert via `grep` that the projection module never reads `client_message_id`. |
| Hard-barrier violation on the server side | Medium | Server emitter (γ-7) emits exactly N+1 envelopes per turn; the existing turn-lifecycle plumbing already serializes terminal events. A unit test on the emitter will assert the invariant. The projection itself drops post-`turn_completed` envelopes silently and counts the drop in `octos_projection_post_completion_drop_total` (see § 14.6 of the spec); operators alert on the metric rather than rehydrating clients. |
| Forward-compat of `Payload` enum | Low | `Payload` is closed in v1 (sealed). Adding a variant is a wire contract change requiring a follow-up UPCR. Clients MUST treat unknown `type` discriminators as a desync signal and rehydrate. |
| Cross-cancel: a cancelled turn's `turn_completed` still fires | Low | The `turn_completed` envelope IS emitted for cancelled turns (spec § 14.6). The lifecycle truth (cancelled vs completed) lives in legacy `turn/error` / `turn/interrupted` notifications until M10 unifies them. |

## Open Questions

- Should `Envelope` carry a top-level `cursor` field for replay
  alignment with `session/open { after: <cursor> }`? **Decision**:
  no for v1 — `(thread_id, seq)` is the projection key and the
  existing `event_cursor` (already documented in § 5) handles
  reconnect replay. Adding a per-envelope cursor would duplicate state
  the cursor index already covers.
- Should `tool_end.status` be an open snake_case enum (matching
  `MessagePersistedSource`) or closed (matching `DiffPreviewLineKind`)?
  **Decision**: closed for v1, and codex M9-γ-1 review extended the
  set from `complete | error` to `complete | error | skipped | aborted`
  (with optional `reason` carrying detail) so deadline-skip and
  user/system-driven cancellation are first-class wire states. Future
  variants still require a follow-up UPCR.
- Should `assistant_persisted` ALSO carry the `cursor` from the
  underlying `MessagePersistedEvent`? **Decision**: no for v1 — the
  `cursor` is for resumable replay over the existing notification
  stream; the projection consumes envelopes from the committed log
  and uses `seq` for ordering. The two cursor namespaces stay
  independent.

## Acceptance Criteria

- [x] Spec § 14 "M9-γ Envelope" lands in
      `api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md`.
- [x] § 4.1 governance entry references this UPCR.
- [x] § 5.1 documents the identity-model collapse and the legacy
      `message_id` deprecation note.
- [x] Rust `Envelope`, `Payload` (with `UserMessage` variant),
      `MessageMeta`, `EnvelopeTokenUsage`, `EnvelopeToolEndStatus`
      (`complete | error | skipped | aborted`), and `FileRef` land in
      `crates/octos-core/src/ui_protocol.rs` with serde
      `tag = "type", content = "data", rename_all = "snake_case"`.
- [x] TS counterparts land in
      `crates/octos-web/src/runtime/ui-protocol-types.ts` and pass
      `tsc --noEmit`.
- [x] Golden round-trip tests in `octos-core::ui_protocol::tests`
      pass (`cargo test -p octos-core`), including coverage for
      `user_message` (with and without files), `assistant_delta`
      omitting `client_message_id`, and `tool_end` with `skipped` /
      `aborted` status.
- [x] Capability flag `projection.envelope.v1` registered in
      `UI_PROTOCOL_KNOWN_FEATURES`.
- [x] No existing tests broken by the additive change (full
      `cargo test -p octos-core` green).
- [x] Status flipped to `accepted` before any γ-2 implementation lands.
