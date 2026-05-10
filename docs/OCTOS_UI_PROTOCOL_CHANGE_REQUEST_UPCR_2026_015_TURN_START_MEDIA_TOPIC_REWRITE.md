# Octos UI Protocol Change Request: `turn/start` media + topic + rewrite_for fields

## Header

- Request id: `UPCR-2026-015`
- Title: Extend `TurnStartParams` with optional `media: Vec<FileRef>`, `topic: Option<String>`, and `rewrite_for: Option<String>` to restore three chat-flow features stubbed out by the M9-α-5/α-6 atomic SSE delete
- Author: M9-β-1 worker (coding-green)
- Date: 2026-05-10
- Target protocol: `octos-ui/v1alpha1`
- Status: accepted
- Related issues: `#834` (web-side α-5/α-6 stubs), `#845` (atomic SSE delete audit), follow-up note in `octos-web-live/src/runtime/ui-protocol-send.ts:81-92`
- Related ADR: M9-α SSE-removal ADR (PR #830)
- Sibling UPCRs: `UPCR-2026-014` (M9-γ projection envelope — defines the `FileRef` shape this UPCR reuses)

## Summary

This change request adds three optional fields to the existing `turn/start` JSON-RPC command's `TurnStartParams` to restore three chat-flow features that the legacy SSE `chatSSE()` body carried but the WebSocket UI Protocol shape (`bridge.sendTurn`) did not:

1. `media: Vec<FileRef>` — pre-uploaded image / voice / file references the user attached to the send. The web client uploads via `POST /api/upload` first; the returned paths are echoed here. Each entry mirrors the canonical `FileRef` shape (`path`, `mime`, `size_bytes`) introduced for `Payload::UserMessage.files` in UPCR-2026-014.
2. `topic: Option<String>` — sub-topic suffix that scopes this send to a per-topic session bucket (`<session>#<topic>` shape). The server folds it into the resolved `SessionKey` before scope validation, so history lookup, ledger appends, and `task/list` filtering all see the right bucket.
3. `rewrite_for: Option<String>` — `client_message_id` of an existing queued user message that this turn replaces in place rather than appending a new turn. Used by the SPA's `/queue` slash-command flow when the user edits a queued prompt before it dispatches.

The change is strictly additive: no existing method, notification, payload, enum variant, required field, or capability bit is modified. Pre-β-1 servers and clients see exactly the bytes they used to.

## Motivation

M9-α-5/α-6 (PR #830) deleted the legacy SSE chat transport (`chatSSE()` + `/api/sessions/{id}/events/stream`). The rationale for the atomic delete was correct — SSE for live text was buggy under M10-class race conditions, and the full WebSocket UI Protocol replaced it for streaming.

But three chat-flow features rode along on the SSE request body that the WS shape `TurnStartInput { kind: "text", text: string }` did not carry:

- `media: MediaRef[]` — image / voice / file attachments
- `topic` — query/body field routing the send to a topic-scoped session bucket
- `rewrite_for` (legacy `request_text` semantic via the `/queue` slash-command) — the server-side rewrite of the queued user prompt

The α-5/α-6 web-side closed PR #834 surfaced this gap by replacing the affected sends with three explicit error strings in `octos-web-live/src/runtime/ui-protocol-send.ts:83-89`:

```text
"media uploads are not yet supported on the WS chat transport
 (follow-up: M9-β extension to TurnStartInput)"
"`/queue`-style rewrites are not yet supported on the WS chat transport
 (follow-up: M9-β extension to TurnStartInput)"
"topic-scoped sends are not yet supported on the WS chat transport
 (follow-up: M9-β extension to session/open / TurnStartInput)"
```

This UPCR is the M9-β-1 follow-up the comments point at: extend the `TurnStartParams` envelope, restore the server plumbing, and remove the three error strings on the web side.

## Change Type

Additive command params. Three optional fields on the existing `turn/start` command. No new methods, notifications, capability flags, or wire-level types. The reused `FileRef` type lands once in UPCR-2026-014 (M9-γ-1, accepted) and is referenced here.

## Wire Contract

Affected wire surface — strictly additive:

- Command params: `TurnStartParams` gains three optional fields (each tagged with serde `skip_serializing_if = "..."` so default values are omitted on the wire).

### `TurnStartParams` (extension)

```jsonc
{
  "session_id": "<SessionKey>",
  "turn_id": "<TurnId>",
  "input": [{ "kind": "text", "text": "..." }],

  // ── M9-β-1 additions (UPCR-2026-015) ────────────────────────────
  "media":       [{ "path": "/tmp/...png", "mime": "image/png", "size_bytes": 4096 }],
  "topic":       "research",
  "rewrite_for": "<client_message_id-of-queued-prompt>"
}
```

All three fields are absent on the wire when at their default (empty `Vec` / `None`). A server that does not understand the additions ignores them; a client that does not populate them sends exactly the bytes a pre-β-1 build would send. This is the strict-additive back-compat contract the M9 merge train relies on.

`FileRef` shape — re-used verbatim from UPCR-2026-014:

```jsonc
{
  "path":       "<server-resolvable filesystem path returned by POST /api/upload>",
  "mime":       "image/png | audio/mpeg | application/pdf | ...",
  "size_bytes": 4096
}
```

### Server-side fold rules

- `topic`: when present and non-empty, the server replaces the resolved `SessionKey` with `<base_key>#<topic>` BEFORE scope validation. Empty / whitespace-only topics fall through to the bare session shape (matching `SessionKey::with_topic`'s own short-circuit). A client that already encoded `#topic` in `session_id` can still send a separate `topic` field — the server splices the existing suffix away first so the canonical form on the wire-trip back to clients is `<base>#<topic>` exactly once.
- `media`: paths are passed verbatim into `Agent::process_message(prompt, history, media_paths)` — the same entry the gateway-mode `ApiChannel` and the `octos chat` CLI use. `process_message` already handles them.
- `rewrite_for`: the β-1 server logs the field at debug level for observability and forwards the prompt through the standard turn pipeline. The durable in-place ledger replace path is a follow-up (sibling issue, deferred per the β-1 scope envelope) — the wire field is locked here so client behaviour is forward-compatible with the deferred persist plumbing.

## Feature Flag

This UPCR does NOT add a capability flag. The fields are strict-additive optionals on an existing command — old clients send bytes that look exactly like a pre-β-1 build (the new fields skip-when-empty), and old servers that don't deserialize the new keys ignore them via serde's default `#[serde(default)]` semantics. A negotiation gate is unnecessary.

## Server impl

Lands in:

- `crates/octos-core/src/ui_protocol.rs` — `TurnStartParams` struct gains the three fields (with serde `skip_serializing_if` annotations); golden round-trip tests cover each field in isolation and all three together.
- `crates/octos-cli/src/api/ui_protocol.rs::handle_turn_start` — folds `params.topic` into `params.session_id` BEFORE `validate_session_scope`; passes `params.media[*].path` into `Agent::process_message` as a `Vec<String>`; logs `params.rewrite_for` at debug level.

Tests landing alongside:

- `cargo test -p octos-core ui_protocol::tests` — four new round-trip tests:
  - `turn_start_round_trips_with_media_field`
  - `turn_start_round_trips_with_topic_field`
  - `turn_start_round_trips_with_rewrite_for_field`
  - `turn_start_round_trips_with_all_beta1_fields`
- `cargo test -p octos-cli api::ui_protocol::tests` — two new acceptance tests:
  - `parses_turn_start_rpc_request_with_beta1_fields`
  - `parses_legacy_turn_start_rpc_request_stays_back_compat`

These lock the wire shape (serde round-trip, snake_case discriminator presence, optional-field omission on the wire when at defaults).

## Client-side

TS counterparts land in `octos-web/src/runtime/ui-protocol-types.ts` and `ui-protocol-bridge.ts`. The three error strings in `ui-protocol-send.ts:83-89` are deleted; sends populate the new fields directly.

## Rollout

- **β-1 (this UPCR)**: Rust struct extension + server fold + TS types + `bridge.sendTurn` plumbing. Status flips to `accepted` once the server PR lands.
- **Follow-up (deferred)**: durable in-place ledger replace for `rewrite_for`. Lives in a sibling β-N issue. The wire field is forward-compatible — when the persist path lands, no client change is required.

## Risks

| Risk | Severity | Mitigation |
|---|---|---|
| `topic` fold collides with a session_id that already encodes `#topic` | Low | The server splices any existing `#suffix` away from the base before re-applying, so the canonical form on the wire-trip back is `<base>#<topic>` exactly once. Test coverage in `parses_turn_start_rpc_request_with_beta1_fields`. |
| `rewrite_for` is wire-locked but ledger-replace is deferred | Low | The field is logged at debug level and the prompt is forwarded through the standard pipeline. From the user's perspective the rewrite is a normal new turn — slightly worse than a true in-place replace, but no functional regression vs the α-5/α-6 stub (which surfaced a hard error). The follow-up that adds durable replace is purely server-side. |
| Old clients send bytes that don't include the new fields | None | Strict-additive shape; serde `#[serde(default)]` + `skip_serializing_if` keep the on-wire form identical to a pre-β-1 build. Verified by `parses_legacy_turn_start_rpc_request_stays_back_compat`. |
| Old servers receive a frame with the new fields | None | Old servers that don't know `media` / `topic` / `rewrite_for` ignore them at deserialize time. The send reduces to a text-only turn, exactly matching pre-β-1 behaviour. |

## Open Questions

- Should `media` carry an optional per-attachment `caption` field? **Decision**: no for v1 — the SPA's existing `localFiles` shape carries a caption that's never read on the server side; restoring server-side carry is out of scope for β-1 (the user-bubble caption is purely client-rendered today).
- Should `rewrite_for` be enforced server-side (reject when no matching queued cmid exists)? **Decision**: no for v1 — best-effort log for now; the durable replace path (deferred follow-up) will add the enforcement when it lands.
- Should `topic` be carried on `session/open` too (so the bridge knows the topic at handshake time)? **Decision**: out of scope for β-1 — the client today opens a bare-session bridge and varies topic per-send. If the active-bridge bookkeeping ever needs topic awareness for a future feature, the additive change there is independent of β-1.

## Acceptance Criteria

- [x] `TurnStartParams` Rust struct gains the three fields with the documented serde annotations.
- [x] `octos-core::ui_protocol::tests` covers the four new round-trip cases.
- [x] `octos-cli::api::ui_protocol::tests` covers the two new acceptance cases (and the legacy-shape back-compat case).
- [x] `handle_turn_start` folds `topic` into `session_id` before scope validation.
- [x] `run_standalone_turn` passes `media[*].path` into `Agent::process_message`.
- [x] `octos-web/src/runtime/ui-protocol-types.ts::TurnStartInput` and `bridge.sendTurn` propagate the new fields.
- [x] The three error strings at `octos-web-live/src/runtime/ui-protocol-send.ts:83-89` are deleted.
- [x] No existing test broken by the additive change (full `cargo test -p octos-core` + `cargo test -p octos-cli --features api --lib` green).
- [x] Status flipped to `accepted` once the server PR lands; web PR opened second per the M9 dependency rule (server wire shape is locked first).
