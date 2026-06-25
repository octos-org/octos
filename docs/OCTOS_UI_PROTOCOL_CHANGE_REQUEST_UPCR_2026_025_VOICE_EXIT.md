# UPCR-2026-025: Voice Exit Intent

Status: accepted
Date: 2026-06-23
PR: TBD

## Summary

Let a voice turn end the voice-assistant session when the user expresses an
end / goodbye / mute intent ("再见", "退出", "拜拜", "静音", "goodbye", …), so the
client leaves the `/voice` screen and returns home — after the turn's farewell
audio finishes playing.

The model appends a trailing `[[EXIT]]` control marker after a short spoken
farewell — **no tool call**, mirroring the `[[VISUAL:kind|brief]]` rich-output
marker (UPCR-2026-024). The marker is an internal control protocol: the backend
strips it from every model-/client-facing surface (live `message/delta`,
persisted `response.content`, assistant carriers) so it never reaches TTS, the
wire, or a later prompt. In its place the server emits one structured
notification: `voice/exit`.

## Decision

Do add one server→client notification — `voice/exit` — carrying the turn-scoped
signal that the voice session should end.

Do let the **client** own the timing of the actual navigation: it leaves
`/voice` only AFTER its reply-audio queue drains, so the spoken farewell is heard
before the screen changes. The event is the trigger; the client gates the
transition on its own playback state.

Do emit the event on the same ledger-backed live path as `file/attached`
(durable append → replayed on reconnect).

Do NOT add a client→server command, a new `TurnState` variant, or a
model-visible tool. The intent is recognised by the model via the in-band
marker; the client only observes the typed event.

Do NOT leak the `[[EXIT]]` marker onto any model- or client-facing surface.

## Capabilities

None. The notification is **ungated** — it is a member of
`UI_PROTOCOL_NOTIFICATION_METHODS` and is advertised in `supported_methods` for
every server slice. A client that does not understand it ignores it
(unknown-notification tolerance); the spoken farewell is unaffected, the user
simply stays on `/voice`.

## AppUI Surface

Defined in the v1 spec § 6 (catalog) and § 8 (event semantics).

### `voice/exit` (event)

The voice turn detected an end / goodbye / mute intent; the client returns home
from `/voice` after the farewell audio finishes. Fields:

- `session_id`, `turn_id` — required turn-scoping.
- `topic` — optional sub-topic routing key.

## Detection & stripping

- Voice-turn system prompt instructs the model to reply with one short farewell
  then append `[[EXIT]]` on its own trailing line — and only when the user truly
  wants to leave (never for ordinary Q&A / chat).
- `parse_exit_marker` accepts the marker only in TRAILING position, so a
  mid-reply mention / quote of the syntax never triggers an exit.
- `strip_exit_directive` lifts + removes the marker from `response.content` and
  every Assistant carrier in `response.messages` before capture / persist / done.
- The streaming `VoiceReplySplitter` (TTS) and `VisibleDeltaFilter`
  (`message/delta`) hold back `[[EXIT]]` as part of the control-marker SET it
  shares with `[[VISUAL:` — ordinary `[[…]]` notation (citations) is still
  spoken.

## Compatibility

Backward-compatible. One new notification method + optional field only; no
command or existing-event change. The event is decoupled from `file/attached`
and `turn/completed`, so the `projection.envelope.v1` cutover (UPCR-2026-014
γ-2) can drop the legacy notifications without disturbing the exit signal.

## Tests

- `crates/octos-core/src/ui_protocol.rs` — `voice/exit` method registration,
  round-trip encode/decode, topic-propagation, and `UI_PROTOCOL_NOTIFICATION_METHODS`
  membership.
- `--features api` `voice_turn` tests — `[[EXIT]]` parse/strip across the live
  delta and persist surfaces, splitter/filter hold-back (incl. token-split and
  false mid-reply marker recovery), and the regression that ordinary `[[1]]`
  citations still reach TTS (the module is feature-gated; the default
  `--workspace` test does not compile it).

## References

- v1 spec § 6 (catalog), § 8 (`voice/exit`).
- `UI_PROTOCOL_NOTIFICATION_METHODS` + `methods::VOICE_EXIT` in
  `crates/octos-core/src/ui_protocol.rs`.
- UPCR-2026-024 (voice rich-output visual lifecycle) — the in-band-marker +
  typed-event pattern this change mirrors.
- Frontend: octos-web `feat/voice-exit-intent` (`voice/exit` → `crew:voice_exit`
  DOM event → voice hook navigates home after audio drains).
