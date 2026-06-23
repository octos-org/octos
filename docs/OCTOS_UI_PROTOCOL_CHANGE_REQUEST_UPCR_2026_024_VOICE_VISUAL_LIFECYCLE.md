# UPCR-2026-024: Voice Rich-Output Visual Lifecycle

Status: accepted
Date: 2026-06-23
PR: #1477

## Summary

Let a voice turn reply with an interactive visual artifact (illustrated HTML /
image / infographic) in addition to speech, and let the client render the
artifact's progress through a typed lifecycle instead of scraping an in-band
marker out of the assistant text.

The model appends a trailing `[[VISUAL:kind|brief]]` control marker after its
spoken reply — **no tool call**, so the Gemini-3 `thought_signature` 400 never
arises and voice turns keep a lean, tool-deferred prefill. The marker is an
internal control protocol: the backend strips it from every model-/client-facing
surface (live `message/delta`, persisted `response.content`, assistant carriers,
snapshot/fork import boundaries, and compaction summaries) so it never replays
into a later prompt. In its place the server drives the client off three
structured notifications: `visual/generating`, `visual/succeeded`,
`visual/failed`.

## Decision

Do add three server→client notifications — `visual/generating`,
`visual/succeeded`, `visual/failed` — carrying a typed visual-artifact
lifecycle for the turn.

Do keep the lifecycle decoupled from `file/attached`: `file/attached` stays a
pure artifact-delivery signal; the client raises and clears the "generating"
placeholder off the `visual/*` events, NOT off `file/attached`. This split is
deliberate so the lifecycle survives a future `projection.envelope.v1` cutover.

Do emit the events on the same ledger-backed live path as `file/attached`
(durable append → replayed on reconnect).

Do NOT add a client→server command, a new `TurnState` variant, or a
model-visible tool. The artifact is produced by a background task the model
requests through the in-band marker; the client only observes.

Do NOT leak the `[[VISUAL:...]]` marker onto any model- or client-facing
surface.

## Capabilities

None. The three notifications are **ungated** — they are members of
`UI_PROTOCOL_NOTIFICATION_METHODS` and are advertised in `supported_methods`
for every server slice. A client that does not understand them ignores them
(unknown-notification tolerance); the spoken reply and `file/attached` delivery
are unaffected.

## AppUI Surface

Defined in the v1 spec § 6 (catalog) and § 8 (event semantics).

### `visual/generating` (event)

A background visual artifact began generating for the turn. Fields:

- `session_id`, `turn_id` — required turn-scoping.
- `kind` — `html` | `illustrated` | `image` | `infographic`.
- `topic` — optional sub-topic routing key.

### `visual/succeeded` (event)

The background visual task produced its artifact(s). The structured success
counterpart of `visual/generating`; the client clears the placeholder off this
event. Emitted alongside `file/attached` on the success branch. Fields:

- `session_id`, `turn_id`, `kind` — as above.
- `files` — workspace-relative filenames of the delivered artifact(s) (the same
  paths carried on the accompanying `file/attached` event(s); omitted when
  empty).
- `topic` — optional.

### `visual/failed` (event)

The background visual task failed, timed out, or was cancelled; the client
clears the placeholder. Fields:

- `session_id`, `turn_id` — required.
- `reason` — optional failure/timeout/cancel detail.
- `topic` — optional.

## Dispatch by kind

- `html` → `octos-agent::rich_output::author_html` writes a self-contained
  interactive document (focused, tool-less authoring call).
- `illustrated` → two-stage: `mofa_image` PNG inlined as a `data:` URI into a
  single self-contained interactive HTML file. The turn's live camera frame is
  forwarded to `mofa_image` as a reference image when the explicit `live_video`
  signal (#1476) is present.
- `image` / `infographic` → backend-orchestrated mofa skill.

Each dispatch runs through `ToolRegistry::execute` (provider policy + arg-size
limit + deferred-tool auto-activation), is registered as a cancelable
`TaskSupervisor` task with a 180s timeout and token-usage logging, and writes a
unique output filename so the mofa skill's path cache cannot return a stale
image across turns.

## Compatibility

Backward-compatible. New notification methods + optional fields only; no command
or existing-event change. The lifecycle is decoupled from `file/attached` so the
`projection.envelope.v1` cutover (UPCR-2026-014 γ-2) can drop the legacy
`message/delta`/`tool/*`/`turn/completed` notifications without disturbing the
visual lifecycle.

Voice-turn `message/delta` streaming dual-emits the canonical projection
envelope (PR #1496), so `projection.envelope.v1` clients receive the sanitized
assistant deltas on voice turns rather than having the bare ephemeral
`message/delta` filtered out.

## Tests

- `crates/octos-core/src/ui_protocol.rs` — `visual/*` method registration,
  round-trip encode/decode, topic-propagation, and `supported_methods`
  membership (28 markers across the suite).
- `--features api` `voice_turn` tests — marker stripping across live delta,
  persist, snapshot/fork, and compaction surfaces (the module is feature-gated;
  the default `--workspace` test does not compile it).

## References

- v1 spec § 6 (catalog), § 8 (`visual/generating`, `visual/succeeded`,
  `visual/failed`).
- `UI_PROTOCOL_NOTIFICATION_METHODS` + `methods::VISUAL_*` in
  `crates/octos-core/src/ui_protocol.rs`.
- UPCR-2026-014 (projection envelope) — the cutover this lifecycle is designed
  to survive.
- #1476 (live-camera context hint), frontend octos-web #232 / #238 / #239.
