//! M9-α-9 — typed envelope helpers for α-7's once-SSE-only events.
//!
//! Per the M9-α (Sole Transport) ADR (`docs/M9-ALPHA-SOLE-TRANSPORT-ADR.md`)
//! the WebSocket transport is the sole chat transport. SSE has been
//! deleted in α-5/α-6 (atomic with the web bundle). The 5 events α-7
//! surfaced as previously-SSE-only have already been migrated to typed
//! v1 envelopes; this module is the canonical home for the helpers
//! that emit those envelopes (turn/started/completed addenda, file/
//! attached, session/event-bridged).
//!
//! Post-α-5/α-6 the helpers are not yet wired into the new WS lifecycle
//! handlers (that's a separate follow-up). They retain their unit tests
//! and will be reused as-is when the lifecycle handlers re-emit them.
//!
//! **Scope per UPCR-2026-014** (the addendum landing in this PR):
//!
//! 1. `session_result` — final session-completion identity (committed_seq +
//!    message_id + client_message_id) for the closing assistant row.
//!    Carried as `TurnCompletedEvent.session_result` so it rides on the
//!    existing `turn/completed` envelope (not a new method).
//! 2. `file_attached` — per-turn file attachment from a tool's
//!    `files_to_send`. Carried as the new `file/attached` envelope.
//! 3. `tokens_in` / `tokens_out` — final token usage for the turn.
//!    Carried as `TurnCompletedEvent.tokens_in/out`.
//! 4. `topic` on `turn/start` — sub-topic suffix for multi-topic specs.
//!    Carried as `TurnStartedEvent.topic`.
//! 5. `/api/sessions/:id/events/stream` — legacy free-form SSE event
//!    stream. Bridged onto a new `session/event` envelope that wraps the
//!    legacy `type` + payload so WS-only clients keep observing each
//!    frame as it gradually lifts onto a typed v1 envelope.
//!
//! **Coexistence invariants** (same as α-2 / α-3 / α-4):
//! - SSE delivery is unchanged — the helpers in this module ONLY append
//!   to the ledger; the SSE wire path runs through whichever channel
//!   reporter / handler emitted the original frame.
//! - Ledger appends are best-effort. A failure does not affect the SSE
//!   path or the agent loop.
//! - WS clients dedupe by stable identity (turn_id + session_id +
//!   committed_seq) so a client connected to both transports collapses
//!   the duplicate into one logical update.
//!
#![allow(dead_code)]

use std::sync::Arc;

use chrono::Utc;
use octos_agent::BackgroundResultPayload;
use octos_core::SessionKey;
use octos_core::ui_protocol::{
    FileAttachedEvent, SessionEventBridgedEvent, TurnCompletedEvent, TurnId, TurnSessionResult,
    TurnStartedEvent, UiNotification,
};
use serde_json::Value;

use super::ui_protocol_ledger::UiProtocolLedger;

/// Append a `turn/started.v1` notification with an optional `topic`
/// suffix, mirroring the SSE-side topic carried on
/// `POST /api/chat?stream=true&topic=…`.
///
/// This is the α-9 replacement for `ui_protocol_alpha3_bridge::emit_turn_started`
/// when a topic must thread through. Callers without a topic should keep
/// using the α-3 helper (it remains the canonical no-topic shape).
///
/// Failure mode: ledger append failures are logged inside the ledger
/// and do not propagate. SSE delivery continues unaffected — that is
/// the explicit α-9 coexistence invariant.
pub(super) fn emit_turn_started_with_topic(
    ledger: &Arc<UiProtocolLedger>,
    session_id: &SessionKey,
    turn_id: &TurnId,
    topic: Option<String>,
) {
    let notification = UiNotification::TurnStarted(TurnStartedEvent {
        session_id: session_id.clone(),
        turn_id: turn_id.clone(),
        timestamp: Utc::now(),
        topic: topic.filter(|t| !t.is_empty()),
    });
    let _ = ledger.append_notification(notification);
}

/// Append a `turn/completed.v1` notification carrying the SSE-side
/// `session_result` identity (committed_seq + message_id +
/// client_message_id) and aggregated token usage onto the ledger.
///
/// Mirrors `emit_turn_completed` from α-3 plus the UPCR-2026-014
/// addendum fields. The ledger overwrites the `cursor` field with the
/// assigned ledger seq via `UiProtocolLedgerEvent::with_cursor` (see
/// `ui_protocol_ledger.rs`), so the placeholder `None` here is the
/// canonical caller-side input.
///
/// `session_result` is `None` when the turn ended without a final
/// assistant row (errored / interrupted before LLM produced text).
/// `tokens_in` / `tokens_out` are `None` when the runtime did not
/// surface usage (rare; happens when no LLM call landed).
pub(super) fn emit_turn_completed_full(
    ledger: &Arc<UiProtocolLedger>,
    session_id: &SessionKey,
    turn_id: &TurnId,
    tokens_in: Option<u32>,
    tokens_out: Option<u32>,
    session_result: Option<TurnSessionResult>,
) {
    let notification = UiNotification::TurnCompleted(TurnCompletedEvent {
        session_id: session_id.clone(),
        topic: None,
        turn_id: turn_id.clone(),
        cursor: None,
        tokens_in,
        tokens_out,
        session_result,
    });
    let _ = ledger.append_notification(notification);
}

/// Append a `file/attached.v1` envelope when a tool surfaces an
/// artifact via `files_to_send`. Mirrors the SSE `file:` frame.
///
/// `topic` is taken as a separate argument (rather than reading from
/// `session_id.topic()`) because the P0-A wire-gap fix in
/// [`emit_files_attached_from_background`] strips the topic suffix
/// from `session_id` BEFORE calling here — the strip is required so
/// the envelope rides on the base broadcast bucket the SPA actually
/// subscribes to. Topic must therefore be captured at the upstream
/// emit site (before stripping) and threaded in explicitly so the
/// event's `topic` field still carries it; otherwise the topic-scoped
/// classifier (`ledger_event_matches_topic_scope`) reads `None` and
/// silently drops the frame. The append-time safety net
/// (`stamp_topic_from_session`) cannot recover because it pulls topic
/// from `session_id`, which has already been stripped. See #1336
/// round-2 BLOCKER for the deep-trace rationale.
///
/// `tool_call_id` is optional because not every file-emission path
/// runs inside a tool execution (rare; reserved for background-result
/// futures). `mime` is also optional — clients fall back to extension
/// sniffing when absent.
pub(super) fn emit_file_attached(
    ledger: &Arc<UiProtocolLedger>,
    session_id: &SessionKey,
    topic: Option<&str>,
    turn_id: &TurnId,
    path: String,
    tool_call_id: Option<String>,
    mime: Option<String>,
) {
    let notification = UiNotification::FileAttached(FileAttachedEvent {
        session_id: session_id.clone(),
        topic: topic
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(ToOwned::to_owned),
        turn_id: turn_id.clone(),
        path,
        tool_call_id,
        mime,
    });
    let _ = ledger.append_notification(notification);
}

/// Emit one `file/attached` envelope per delivered artefact when a
/// `spawn_only` background tool's `BackgroundResultPayload` lands on
/// the AppUI WS path.
///
/// Coalesces the payload's persist-media (`media`, lives on the
/// `message/persisted` row) and envelope-only-media (`envelope_media`,
/// surfaced on the `turn/spawn_complete` envelope) into a single
/// deduplicated stream of paths — clients receive ONE `file/attached`
/// per unique artefact regardless of which path source carried it. A
/// path that appears in both sources still emits exactly one envelope.
///
/// Defensive against the production failure mode the slides soak
/// captured (2026-05-24): PPTX artefacts that landed on disk and were
/// verified by the workspace contract never surfaced a clickable
/// button on the SPA because `turn/spawn_complete` and the
/// `message/persisted` row's `media` field both required the SPA's
/// content-bearing-envelope reducers to fire correctly. A dedicated
/// per-file envelope is the redundant signal that keeps the user-
/// visible delivery resilient against placement / sticky-thread bugs
/// in those richer reducers.
///
/// Best-effort: ledger append failures are logged inside the ledger
/// and do not propagate. Callers MUST run this after the persist /
/// `turn/spawn_complete` block so the placement context (turn_id,
/// session_id) is stable. No-op when both source lists are empty.
///
/// **P0-A wire-gap (2026-05-26):** the helper strips any `#<topic>`
/// suffix from the incoming `session_id` before publishing each
/// envelope onto the ledger. The SPA subscribes only to the BASE
/// SessionKey via `session/open`; `handle_turn_start` folds the topic
/// into `params.session_id` but never re-subscribes. Result: every
/// `file_attached` event published on a topic-suffixed broadcast
/// bucket fans out to zero subscribers. Publishing on the base key
/// keeps the live subscribers (and replay) receiving the envelope.
///
/// The SPA dedupes by `tool_call_id` client-side, so the loss of
/// topic info on the wire has no UX cost — see the rationale in
/// 35a68cb9 (the topic-scope-filter exemption companion fix).
pub(super) fn emit_files_attached_from_background(
    ledger: &Arc<UiProtocolLedger>,
    session_id: &SessionKey,
    turn_id: &TurnId,
    media: &[String],
    envelope_media: &[String],
    tool_call_id: Option<String>,
) {
    use std::collections::BTreeSet;
    // #1336 round-2: capture topic BEFORE stripping the suffix below.
    // The strip is required for routing (SPA subscribes on base only),
    // but the event itself must still carry the topic so the
    // topic-scoped classifier accepts it. See [`emit_file_attached`]
    // for the full rationale.
    let topic = session_id.topic().map(ToOwned::to_owned);
    // P0-A wire-gap fix: publish on the base SessionKey (no `#<topic>`
    // suffix). The SPA's `session/open` subscription is keyed on the
    // base form; topic-suffixed broadcasts reach zero live subscribers.
    let base_session = SessionKey(session_id.base_key().to_owned());
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for path in media.iter().chain(envelope_media.iter()) {
        if path.is_empty() {
            continue;
        }
        if !seen.insert(path.clone()) {
            continue;
        }
        let mime = mime_from_path(path);
        emit_file_attached(
            ledger,
            &base_session,
            topic.as_deref(),
            turn_id,
            path.clone(),
            tool_call_id.clone(),
            mime,
        );
    }
}

/// Lightweight extension-based MIME sniffer used by
/// [`emit_files_attached_from_background`].
///
/// `file/attached` clients render attachments from the `mime` hint
/// when present and fall back to extension parsing otherwise. We
/// populate it with the artefact families the spawn_only producers
/// actually emit — `.pptx`, `.html`, `.md`, `.mp3`, `.mp4`, `.png`,
/// `.jpg`. Anything else returns `None` and clients fall back to
/// extension-based rendering. We deliberately do NOT crack open the
/// file (no I/O on the dispatch hot path); the wire shape is best-
/// effort so extension drift is recovered by the client.
fn mime_from_path(path: &str) -> Option<String> {
    let lower = path.to_lowercase();
    let suffix = lower.rsplit('.').next()?;
    let mime = match suffix {
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "html" | "htm" => "text/html",
        "md" | "markdown" => "text/markdown",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "mp4" => "video/mp4",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "json" => "application/json",
        "zip" => "application/zip",
        _ => return None,
    };
    Some(mime.to_owned())
}

/// Resolve the effective envelope-media list for a spawn_only background
/// completion payload.
///
/// The `BackgroundResultPayload` shape carries TWO media lists by design
/// (see `BackgroundResultPayload::envelope_media` doc):
///
/// - `media` lands on the `message/persisted` row for the completion
///   (legacy carrier old clients render). Populated by the contract
///   `Satisfied` path with `output_files`; left empty by the
///   `NotConfigured` `send_file` fallback because each delivered file
///   already has its own per-file `message/persisted` companion row.
/// - `envelope_media` surfaces ONLY on the `turn/spawn_complete`
///   envelope (the wire signal dual-negotiated clients consume after
///   they suppress the per-file companions). Populated by the
///   `NotConfigured` `send_file` fallback with `sent_files`; left
///   empty by the `Satisfied` path because `media` already carries the
///   list.
///
/// The two carriers were split so the same producer can serve old and
/// new clients without one shape double-rendering the artefacts the
/// other already covered. The cost is that EVERY consumer that needs
/// to render attachments has to coalesce the two: pre-helper, the
/// `BackgroundResultSender` closure inlined an `if envelope_media
/// is_empty { media } else { envelope_media }` fallback, and a future
/// caller that forgot to mirror that fallback would silently drop
/// half of the live spawn-only completion shapes.
///
/// This helper centralises the fallback so the `turn/spawn_complete`
/// envelope builder, the `emit_files_attached_from_background` caller,
/// and future consumers all see the same effective list. Returning a
/// fresh `Vec<String>` (rather than a borrowed slice) keeps callers
/// free to retain ownership when both sources outlive the call site.
///
/// The slides soak round-13 (2026-05-25) confirmed the production
/// shape: `mofa_slides` enters the `Satisfied` branch with
/// `media: [deck.pptx]` and `envelope_media: []`. Without the
/// fallback, `turn/spawn_complete` and `file/attached` consumers both
/// see an empty list and the SPA never renders a download button.
pub(super) fn effective_envelope_media(payload: &BackgroundResultPayload) -> Vec<String> {
    if payload.envelope_media.is_empty() {
        payload.media.clone()
    } else {
        payload.envelope_media.clone()
    }
}

/// Bridge a legacy `/api/sessions/:id/events/stream` SSE frame onto the
/// WS surface as a `session/event.v1` envelope.
///
/// `kind` is the legacy SSE `type` field (e.g. `"replay_complete"`,
/// `"task_started"`); `payload` is the full frame body. `topic` is
/// extracted from the frame for client-side scoping (avoids parsing
/// `payload`). The legacy stream is free-form by design — this wrapper
/// keeps WS-only clients observing every signal SSE consumers see while
/// each event kind gradually migrates to a typed v1 envelope.
pub(super) fn emit_session_event(
    ledger: &Arc<UiProtocolLedger>,
    session_id: &SessionKey,
    kind: String,
    payload: Value,
    topic: Option<String>,
) {
    let notification = UiNotification::SessionEventBridged(SessionEventBridgedEvent {
        session_id: session_id.clone(),
        kind,
        payload,
        topic: topic.filter(|t| !t.is_empty()),
    });
    let _ = ledger.append_notification(notification);
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_core::ui_protocol::methods;
    use serde_json::json;

    /// α-9 acceptance gate (1) — `topic` lands on `turn/started`.
    #[test]
    fn should_emit_turn_started_with_topic_field() {
        let ledger = Arc::new(UiProtocolLedger::new(64));
        let session_id = SessionKey::new("api", "alpha9-topic");
        let turn_id = TurnId::new();
        let mut subscriber = ledger.subscribe(&session_id);

        emit_turn_started_with_topic(&ledger, &session_id, &turn_id, Some("slides".into()));

        let event = subscriber.try_recv().expect("turn/started broadcasts");
        match &event.event {
            crate::api::ui_protocol_ledger::UiProtocolLedgerEvent::Notification(
                UiNotification::TurnStarted(payload),
            ) => {
                assert_eq!(payload.session_id, session_id);
                assert_eq!(payload.turn_id, turn_id);
                assert_eq!(payload.topic.as_deref(), Some("slides"));
            }
            other => panic!("expected TurnStarted, got {other:?}"),
        }
        // Method name unchanged from α-3 — the addendum is field-only.
        let rpc = event
            .event
            .clone()
            .into_rpc_notification()
            .expect("serializes");
        assert_eq!(rpc.method, methods::TURN_STARTED);
    }

    /// α-9 acceptance gate (1b) — empty topic strings collapse to None
    /// so the `skip_serializing_if` keeps the wire shape identical to
    /// α-3 for no-topic turns. Without this, every turn-start envelope
    /// would carry a `"topic": ""` field, regressing α-3 wire-shape
    /// goldens.
    #[test]
    fn should_collapse_empty_topic_to_none() {
        let ledger = Arc::new(UiProtocolLedger::new(64));
        let session_id = SessionKey::new("api", "alpha9-empty-topic");
        let turn_id = TurnId::new();
        let mut subscriber = ledger.subscribe(&session_id);

        emit_turn_started_with_topic(&ledger, &session_id, &turn_id, Some(String::new()));

        let event = subscriber.try_recv().expect("turn/started broadcasts");
        if let crate::api::ui_protocol_ledger::UiProtocolLedgerEvent::Notification(
            UiNotification::TurnStarted(payload),
        ) = &event.event
        {
            assert!(payload.topic.is_none(), "empty topic must collapse to None");
        }
    }

    /// α-9 acceptance gate (2) — `tokens_in/out` + `session_result`
    /// land on `turn/completed`.
    #[test]
    fn should_emit_turn_completed_with_tokens_and_session_result() {
        let ledger = Arc::new(UiProtocolLedger::new(64));
        let session_id = SessionKey::new("api", "alpha9-completed-rich");
        let turn_id = TurnId::new();
        let mut subscriber = ledger.subscribe(&session_id);

        emit_turn_completed_full(
            &ledger,
            &session_id,
            &turn_id,
            Some(1234),
            Some(567),
            Some(TurnSessionResult {
                committed_seq: 42,
                message_id: format!("{}:42:1700000000", session_id.0),
                client_message_id: Some("cmid-alpha-9".into()),
            }),
        );

        let event = subscriber.try_recv().expect("turn/completed broadcasts");
        match &event.event {
            crate::api::ui_protocol_ledger::UiProtocolLedgerEvent::Notification(
                UiNotification::TurnCompleted(payload),
            ) => {
                assert_eq!(payload.session_id, session_id);
                assert_eq!(payload.turn_id, turn_id);
                assert_eq!(payload.tokens_in, Some(1234));
                assert_eq!(payload.tokens_out, Some(567));
                let sr = payload
                    .session_result
                    .as_ref()
                    .expect("session_result populated");
                assert_eq!(sr.committed_seq, 42);
                assert_eq!(sr.client_message_id.as_deref(), Some("cmid-alpha-9"));
                // Ledger stamps cursor onto turn/completed (UPCR-2026-007).
                let cursor = payload.cursor.as_ref().expect("cursor stamped");
                assert!(cursor.seq > 0);
                assert_eq!(cursor.stream, session_id.0);
            }
            other => panic!("expected TurnCompleted, got {other:?}"),
        }
    }

    /// α-9 acceptance gate (2b) — None values must collapse to omitted
    /// fields so legacy clients (pre-addendum) deserialize the envelope
    /// unchanged. Without this, the addendum would force every
    /// turn/completed wire frame to carry the new fields.
    #[test]
    fn should_omit_addendum_fields_when_none() {
        let ledger = Arc::new(UiProtocolLedger::new(64));
        let session_id = SessionKey::new("api", "alpha9-completed-none");
        let turn_id = TurnId::new();
        let mut subscriber = ledger.subscribe(&session_id);

        emit_turn_completed_full(&ledger, &session_id, &turn_id, None, None, None);

        let event = subscriber.try_recv().expect("turn/completed broadcasts");
        let rpc = event
            .event
            .clone()
            .into_rpc_notification()
            .expect("serializes");
        let params = rpc.params;
        // None fields must be absent from the wire object.
        assert!(
            !params.as_object().unwrap().contains_key("tokens_in"),
            "tokens_in absent on None"
        );
        assert!(
            !params.as_object().unwrap().contains_key("tokens_out"),
            "tokens_out absent on None"
        );
        assert!(
            !params.as_object().unwrap().contains_key("session_result"),
            "session_result absent on None"
        );
    }

    /// α-9 acceptance gate (3) — `file/attached` envelope round-trips
    /// the path, tool_call_id, and mime through the ledger broadcast
    /// with the expected wire method.
    #[test]
    fn should_emit_file_attached_with_full_payload() {
        let ledger = Arc::new(UiProtocolLedger::new(64));
        let session_id = SessionKey::new("api", "alpha9-file-attached");
        let turn_id = TurnId::new();
        let mut subscriber = ledger.subscribe(&session_id);

        emit_file_attached(
            &ledger,
            &session_id,
            None,
            &turn_id,
            "/tmp/output.png".into(),
            Some("tc-1".into()),
            Some("image/png".into()),
        );

        let event = subscriber.try_recv().expect("file/attached broadcasts");
        match &event.event {
            crate::api::ui_protocol_ledger::UiProtocolLedgerEvent::Notification(
                UiNotification::FileAttached(payload),
            ) => {
                assert_eq!(payload.session_id, session_id);
                assert_eq!(payload.turn_id, turn_id);
                assert_eq!(payload.path, "/tmp/output.png");
                assert_eq!(payload.tool_call_id.as_deref(), Some("tc-1"));
                assert_eq!(payload.mime.as_deref(), Some("image/png"));
            }
            other => panic!("expected FileAttached, got {other:?}"),
        }
        let rpc = event
            .event
            .clone()
            .into_rpc_notification()
            .expect("serializes");
        assert_eq!(rpc.method, methods::FILE_ATTACHED);
    }

    /// α-9 acceptance gate (3b) — bare path with no tool_call_id / mime
    /// preserves the optionality on the wire.
    #[test]
    fn should_emit_file_attached_with_minimal_payload() {
        let ledger = Arc::new(UiProtocolLedger::new(64));
        let session_id = SessionKey::new("api", "alpha9-file-attached-min");
        let turn_id = TurnId::new();
        let mut subscriber = ledger.subscribe(&session_id);

        emit_file_attached(
            &ledger,
            &session_id,
            None,
            &turn_id,
            "/tmp/bare.txt".into(),
            None,
            None,
        );

        let event = subscriber.try_recv().expect("file/attached broadcasts");
        let rpc = event
            .event
            .clone()
            .into_rpc_notification()
            .expect("serializes");
        let params = rpc.params;
        assert_eq!(params["path"], "/tmp/bare.txt");
        assert!(
            !params.as_object().unwrap().contains_key("tool_call_id"),
            "tool_call_id absent when None"
        );
        assert!(
            !params.as_object().unwrap().contains_key("mime"),
            "mime absent when None"
        );
    }

    /// α-9 acceptance gate (4) — `session/event` wraps a legacy SSE
    /// frame's `type` + payload + topic onto the WS surface.
    #[test]
    fn should_emit_session_event_wrapping_legacy_sse_frame() {
        let ledger = Arc::new(UiProtocolLedger::new(64));
        let session_id = SessionKey::new("api", "alpha9-session-event");
        let mut subscriber = ledger.subscribe(&session_id);

        let payload = json!({
            "type": "replay_complete",
            "topic": "slides",
        });
        emit_session_event(
            &ledger,
            &session_id,
            "replay_complete".into(),
            payload.clone(),
            Some("slides".into()),
        );

        let event = subscriber.try_recv().expect("session/event broadcasts");
        match &event.event {
            crate::api::ui_protocol_ledger::UiProtocolLedgerEvent::Notification(
                UiNotification::SessionEventBridged(p),
            ) => {
                assert_eq!(p.session_id, session_id);
                assert_eq!(p.kind, "replay_complete");
                assert_eq!(p.payload, payload);
                assert_eq!(p.topic.as_deref(), Some("slides"));
            }
            other => panic!("expected SessionEventBridged, got {other:?}"),
        }
        let rpc = event
            .event
            .clone()
            .into_rpc_notification()
            .expect("serializes");
        assert_eq!(rpc.method, methods::SESSION_EVENT);
    }

    /// α-9 acceptance gate (5) — bridge calls route to the SessionKey
    /// they were given, not to a different session. Without this, a
    /// multi-session process (the standard `octos serve` shape) would
    /// cross-deliver bridged frames between concurrently-active turns.
    #[test]
    fn should_route_bridged_envelopes_to_caller_session_only() {
        let ledger = Arc::new(UiProtocolLedger::new(64));
        let session_a = SessionKey::new("api", "alpha9-iso-A");
        let session_b = SessionKey::new("api", "alpha9-iso-B");
        let turn_id = TurnId::new();
        let mut sub_a = ledger.subscribe(&session_a);
        let mut sub_b = ledger.subscribe(&session_b);

        // Fire all four envelope helpers on session_a.
        emit_turn_started_with_topic(&ledger, &session_a, &turn_id, Some("isol".into()));
        emit_turn_completed_full(
            &ledger,
            &session_a,
            &turn_id,
            Some(10),
            Some(20),
            Some(TurnSessionResult {
                committed_seq: 1,
                message_id: format!("{}:1:0", session_a.0),
                client_message_id: None,
            }),
        );
        emit_file_attached(
            &ledger,
            &session_a,
            None,
            &turn_id,
            "/tmp/x".into(),
            None,
            None,
        );
        emit_session_event(
            &ledger,
            &session_a,
            "replay_complete".into(),
            json!({}),
            None,
        );

        // Session A receives all four envelopes.
        let mut count_a = 0;
        while sub_a.try_recv().is_ok() {
            count_a += 1;
        }
        assert_eq!(count_a, 4, "session A receives all four envelopes");

        // Session B receives nothing — no cross-delivery.
        assert!(
            sub_b.try_recv().is_err(),
            "α-9 envelopes must NOT cross-deliver to other session subscribers"
        );
    }

    /// Slides soak regression — `emit_files_attached_from_background`
    /// emits one envelope per unique path across the persist-media and
    /// envelope-only-media lists. Deduplicates paths that appear in
    /// both lists so dual-negotiated clients receive ONE button per
    /// artefact regardless of which carrier brought it.
    #[test]
    fn should_emit_one_file_attached_per_unique_artefact_path() {
        let ledger = Arc::new(UiProtocolLedger::new(64));
        let session_id = SessionKey::new("api", "alpha9-slides-soak");
        let turn_id = TurnId::new();
        let mut subscriber = ledger.subscribe(&session_id);

        // Slides Satisfied path: persist-media carries the deck, envelope
        // path inherits the same list (see api/ui_protocol.rs
        // envelope_media fallback). Soak captured this exact shape.
        let media = vec![
            "/Users/cloud/.octos/profiles/dspfac/data/slides/deck-soak/output/deck.pptx"
                .to_string(),
        ];
        let envelope_media = media.clone();

        emit_files_attached_from_background(
            &ledger,
            &session_id,
            &turn_id,
            &media,
            &envelope_media,
            Some("tc-slides-soak".into()),
        );

        let mut envelopes = Vec::new();
        while let Ok(event) = subscriber.try_recv() {
            if let crate::api::ui_protocol_ledger::UiProtocolLedgerEvent::Notification(
                UiNotification::FileAttached(payload),
            ) = &event.event
            {
                envelopes.push(payload.clone());
            }
        }
        assert_eq!(
            envelopes.len(),
            1,
            "duplicate paths across media + envelope_media collapse to one envelope"
        );
        assert_eq!(envelopes[0].path, media[0]);
        assert_eq!(envelopes[0].tool_call_id.as_deref(), Some("tc-slides-soak"));
        assert_eq!(
            envelopes[0].mime.as_deref(),
            Some("application/vnd.openxmlformats-officedocument.presentationml.presentation"),
            "PPTX extension lifts to the canonical OOXML MIME"
        );
    }

    /// Multi-artefact delivery (e.g. deep_research `_report.md` +
    /// `outline.json`) emits one envelope per file, with stable order
    /// matching the iteration order of the input slices. Empty entries
    /// are filtered (defensive against producers that emit a sentinel
    /// blank path).
    #[test]
    fn should_emit_envelope_per_distinct_path_filtering_blanks() {
        let ledger = Arc::new(UiProtocolLedger::new(64));
        let session_id = SessionKey::new("api", "alpha9-multi-artefact");
        let turn_id = TurnId::new();
        let mut subscriber = ledger.subscribe(&session_id);

        let media = vec![
            "/tmp/report.md".to_string(),
            "".to_string(), // blank sentinel — must be filtered
            "/tmp/outline.json".to_string(),
        ];
        let envelope_media: Vec<String> = vec![];

        emit_files_attached_from_background(
            &ledger,
            &session_id,
            &turn_id,
            &media,
            &envelope_media,
            None,
        );

        let mut paths = Vec::new();
        while let Ok(event) = subscriber.try_recv() {
            if let crate::api::ui_protocol_ledger::UiProtocolLedgerEvent::Notification(
                UiNotification::FileAttached(payload),
            ) = &event.event
            {
                paths.push(payload.path.clone());
            }
        }
        assert_eq!(
            paths,
            vec![
                "/tmp/report.md".to_string(),
                "/tmp/outline.json".to_string()
            ],
            "blank entries filtered; ordering preserved"
        );
    }

    /// No-op fast path — both source lists empty produces zero
    /// envelopes. Text-only background completions (mofa_publish URL
    /// emission, etc.) must NOT clutter the wire with empty
    /// `file/attached` frames.
    #[test]
    fn should_not_emit_when_both_media_lists_empty() {
        let ledger = Arc::new(UiProtocolLedger::new(64));
        let session_id = SessionKey::new("api", "alpha9-empty");
        let turn_id = TurnId::new();
        let mut subscriber = ledger.subscribe(&session_id);

        emit_files_attached_from_background(&ledger, &session_id, &turn_id, &[], &[], None);

        assert!(
            subscriber.try_recv().is_err(),
            "text-only background completions emit zero file/attached envelopes"
        );
    }

    // ─────────────────────────────────────────────────────────────────────
    // Slides soak round-13 (2026-05-25) regression coverage:
    // `BackgroundResultPayload`-shape → `file/attached` end-to-end. The
    // existing helper tests above feed `media` / `envelope_media` lists
    // directly, which exercises `emit_files_attached_from_background`
    // itself but bypasses the closure-internal `effective_envelope_media`
    // fallback. Round-13 captured `.pptx` artefacts on disk + persisted
    // bubbles on the SPA but ZERO `file/attached` frames on 4/4 hosts.
    // The bridge wire is intact; these tests pin that the live
    // spawn_only completion shapes — Satisfied (carrier = `media`) and
    // NotConfigured (carrier = `envelope_media`) — both round-trip a
    // `file/attached` envelope per artefact through the helper layer.
    // ─────────────────────────────────────────────────────────────────────

    /// Helper: build a `BackgroundResultPayload` mirroring the production
    /// spawn_only completion shape so the regression tests don't repeat
    /// ten field literals. Callers fill in the two media lists and the
    /// task_label/tool_call_id; defaults for everything else mirror the
    /// shape the live closures produce.
    fn build_payload(
        task_label: &str,
        tool_call_id: &str,
        media: Vec<String>,
        envelope_media: Vec<String>,
    ) -> BackgroundResultPayload {
        BackgroundResultPayload {
            task_label: task_label.to_string(),
            content: format!("✓ {task_label} completed"),
            kind: octos_agent::BackgroundResultKind::Notification,
            media,
            envelope_media,
            originating_thread_id: Some("test-thread".into()),
            task_id: Some("test-task".into()),
            originating_client_message_id: Some("test-cmid".into()),
            tool_call_id: Some(tool_call_id.to_string()),
        }
    }

    /// `mofa_slides` Satisfied shape — the workspace contract returned
    /// `Satisfied { output_files: [deck.pptx] }`, so `execution.rs` builds
    /// the payload with `media: output_files` and `envelope_media: vec![]`.
    /// The closure-internal fallback in `BackgroundResultSender` (now
    /// extracted into [`effective_envelope_media`]) must lift `media` onto
    /// the envelope side so `emit_files_attached_from_background` sees a
    /// non-empty list AND fires one `file/attached` per artefact.
    ///
    /// Round-13 evidence: the deck reached disk and the persisted bubble
    /// rendered, but `file/attached` count was 0 on all 4 hosts. Without
    /// the fallback, this is exactly the failure mode — the helper sees
    /// `envelope_media: []` and emits nothing for the Satisfied path.
    #[test]
    fn should_emit_file_attached_for_satisfied_spawn_only_payload_shape() {
        let pptx_path =
            "/Users/cloud/.octos/profiles/dspfac/data/slides/round13/output/deck.pptx".to_string();
        let payload = build_payload(
            "mofa_slides",
            "tc-slides-satisfied",
            // Satisfied path: media carries the deck, envelope_media empty.
            vec![pptx_path.clone()],
            vec![],
        );

        // Closure mapping: lift media onto envelope side when the carrier
        // is empty. This is what `BackgroundResultSender` does today and
        // what was previously inlined.
        let envelope_media = effective_envelope_media(&payload);
        assert_eq!(
            envelope_media,
            vec![pptx_path.clone()],
            "Satisfied shape must round-trip media → envelope_media"
        );

        let ledger = Arc::new(UiProtocolLedger::new(64));
        let session_id = SessionKey::new("api", "alpha9-satisfied-shape");
        let turn_id = TurnId::new();
        let mut subscriber = ledger.subscribe(&session_id);

        emit_files_attached_from_background(
            &ledger,
            &session_id,
            &turn_id,
            &payload.media,
            &envelope_media,
            payload.tool_call_id.clone(),
        );

        let mut envelopes = Vec::new();
        while let Ok(event) = subscriber.try_recv() {
            if let crate::api::ui_protocol_ledger::UiProtocolLedgerEvent::Notification(
                UiNotification::FileAttached(p),
            ) = &event.event
            {
                envelopes.push(p.clone());
            }
        }
        assert_eq!(
            envelopes.len(),
            1,
            "the Satisfied-shape payload must produce exactly one file/attached envelope"
        );
        assert_eq!(envelopes[0].path, pptx_path);
        assert_eq!(
            envelopes[0].tool_call_id.as_deref(),
            Some("tc-slides-satisfied"),
        );
        assert_eq!(
            envelopes[0].mime.as_deref(),
            Some("application/vnd.openxmlformats-officedocument.presentationml.presentation"),
            "PPTX MIME hint round-trips so the SPA can render a download button without re-sniffing"
        );
    }

    /// NotConfigured-with-files shape — `execution.rs` ran the per-file
    /// `send_file` retry loop and built the payload with `media: vec![]`
    /// (no persist-media; per-file companion rows already cover the
    /// `message/persisted` carrier) and `envelope_media:
    /// sent_files.clone()`. The fallback is a no-op in this shape
    /// (envelope_media already non-empty) and the helper must still emit
    /// one `file/attached` per delivered file. The per-file
    /// `message/persisted` companion rows the `send_file` consumer
    /// already committed are out of scope here — the new envelope is the
    /// dedicated wire signal dual-negotiated clients consume regardless
    /// of those companions.
    #[test]
    fn should_emit_file_attached_for_not_configured_send_file_shape() {
        let pptx_path =
            "/Users/cloud/.octos/profiles/dspfac/data/slides/round13/output/deck.pptx".to_string();
        let payload = build_payload(
            "mofa_slides",
            "tc-slides-notconfigured",
            // NotConfigured `send_file` fallback: media empty, sent_files
            // surface only on envelope_media so the `message/persisted`
            // row stays byte-identical to the legacy "spawn-ack with
            // text only" shape old clients render.
            vec![],
            vec![pptx_path.clone()],
        );

        let envelope_media = effective_envelope_media(&payload);
        assert_eq!(
            envelope_media,
            vec![pptx_path.clone()],
            "NotConfigured shape preserves envelope_media verbatim (no fallback needed)"
        );

        let ledger = Arc::new(UiProtocolLedger::new(64));
        let session_id = SessionKey::new("api", "alpha9-notconfigured-shape");
        let turn_id = TurnId::new();
        let mut subscriber = ledger.subscribe(&session_id);

        emit_files_attached_from_background(
            &ledger,
            &session_id,
            &turn_id,
            &payload.media,
            &envelope_media,
            payload.tool_call_id.clone(),
        );

        let mut envelopes = Vec::new();
        while let Ok(event) = subscriber.try_recv() {
            if let crate::api::ui_protocol_ledger::UiProtocolLedgerEvent::Notification(
                UiNotification::FileAttached(p),
            ) = &event.event
            {
                envelopes.push(p.clone());
            }
        }
        assert_eq!(
            envelopes.len(),
            1,
            "the NotConfigured-shape payload must produce exactly one file/attached envelope"
        );
        assert_eq!(envelopes[0].path, pptx_path);
        assert_eq!(
            envelopes[0].tool_call_id.as_deref(),
            Some("tc-slides-notconfigured"),
        );
    }

    /// Multi-artefact NotConfigured shape — e.g. a future spawn_only
    /// tool that delivers two PPTX files via the per-file `send_file`
    /// retry loop. The helper must emit one `file/attached` per distinct
    /// path in the order they appear in `envelope_media`, with each
    /// envelope carrying the correct MIME hint.
    #[test]
    fn should_emit_file_attached_once_per_distinct_envelope_media_entry() {
        let primary =
            "/Users/cloud/.octos/profiles/dspfac/data/slides/round13/output/deck.pptx".to_string();
        let alt = "/Users/cloud/.octos/profiles/dspfac/data/slides/round13/output/deck-alt.pptx"
            .to_string();
        let payload = build_payload(
            "mofa_slides",
            "tc-slides-multi",
            vec![],
            vec![primary.clone(), alt.clone()],
        );

        let envelope_media = effective_envelope_media(&payload);
        let ledger = Arc::new(UiProtocolLedger::new(64));
        let session_id = SessionKey::new("api", "alpha9-multi-shape");
        let turn_id = TurnId::new();
        let mut subscriber = ledger.subscribe(&session_id);

        emit_files_attached_from_background(
            &ledger,
            &session_id,
            &turn_id,
            &payload.media,
            &envelope_media,
            payload.tool_call_id.clone(),
        );

        let mut paths = Vec::new();
        while let Ok(event) = subscriber.try_recv() {
            if let crate::api::ui_protocol_ledger::UiProtocolLedgerEvent::Notification(
                UiNotification::FileAttached(p),
            ) = &event.event
            {
                paths.push(p.path.clone());
            }
        }
        assert_eq!(
            paths,
            vec![primary, alt],
            "iteration order across envelope_media must be preserved on the wire"
        );
    }

    /// Text-only spawn_only completion (e.g. `mofa_publish` returning a
    /// deploy URL with no on-disk artefact) — both media lists empty.
    /// [`effective_envelope_media`] returns an empty list and the helper
    /// must NOT emit any `file/attached` frames so the wire stays clean
    /// for text-only branches.
    #[test]
    fn should_not_emit_file_attached_for_text_only_background_payload() {
        let payload = build_payload("mofa_publish", "tc-publish-text-only", vec![], vec![]);

        let envelope_media = effective_envelope_media(&payload);
        assert!(
            envelope_media.is_empty(),
            "text-only completions must coalesce to an empty effective envelope_media"
        );

        let ledger = Arc::new(UiProtocolLedger::new(64));
        let session_id = SessionKey::new("api", "alpha9-text-only-shape");
        let turn_id = TurnId::new();
        let mut subscriber = ledger.subscribe(&session_id);

        emit_files_attached_from_background(
            &ledger,
            &session_id,
            &turn_id,
            &payload.media,
            &envelope_media,
            payload.tool_call_id.clone(),
        );

        assert!(
            subscriber.try_recv().is_err(),
            "text-only completions emit zero file/attached envelopes"
        );
    }

    /// `envelope_media` overrides `media` when both are populated — this
    /// case does not arise from any current branch in `execution.rs`
    /// (Satisfied populates only `media`; NotConfigured populates only
    /// `envelope_media`) but the contract is documented and we pin it so
    /// a future branch that sets both can't silently drift the helper's
    /// behaviour. `emit_files_attached_from_background` still chains both
    /// lists (then dedupes), so the union of paths surfaces; the
    /// `effective_envelope_media` return value is what
    /// `turn/spawn_complete` carries.
    #[test]
    fn effective_envelope_media_prefers_envelope_field_when_both_populated() {
        let primary_persist = "/tmp/persisted.md".to_string();
        let primary_envelope = "/tmp/envelope.pptx".to_string();
        let payload = build_payload(
            "future_tool",
            "tc-future",
            vec![primary_persist],
            vec![primary_envelope.clone()],
        );

        let envelope_media = effective_envelope_media(&payload);
        assert_eq!(
            envelope_media,
            vec![primary_envelope],
            "non-empty envelope_media wins — must NOT silently inherit `media`"
        );
    }

    /// MIME sniffer table — covers the spawn_only artefact families
    /// (`.pptx`, `.md`, `.mp3`, `.html`, image / video) and falls back
    /// to None for unknown extensions so the client can do its own
    /// detection.
    #[test]
    fn should_lift_known_extensions_to_canonical_mime_types() {
        assert_eq!(
            mime_from_path("/abs/path/deck.pptx").as_deref(),
            Some("application/vnd.openxmlformats-officedocument.presentationml.presentation"),
        );
        assert_eq!(
            mime_from_path("/abs/path/REPORT.MD").as_deref(),
            Some("text/markdown"),
            "case-insensitive on extension"
        );
        assert_eq!(
            mime_from_path("/abs/path/podcast.mp3").as_deref(),
            Some("audio/mpeg"),
        );
        assert_eq!(
            mime_from_path("/abs/path/page.html").as_deref(),
            Some("text/html"),
        );
        assert_eq!(
            mime_from_path("/abs/path/page.htm").as_deref(),
            Some("text/html"),
            "htm alias maps to text/html"
        );
        assert_eq!(
            mime_from_path("/abs/path/something.xyz"),
            None,
            "unknown extensions fall back to None"
        );
        assert_eq!(
            mime_from_path("noextension"),
            None,
            "files without extension fall back to None"
        );
    }

    // ─────────────────────────────────────────────────────────────────────
    // P0-A REAL fix — wire-gap regression coverage (2026-05-26).
    //
    // Deep-trace found that `emit_files_attached_from_background` was
    // publishing each `file/attached` envelope onto a topic-suffixed
    // SessionKey broadcast bucket (e.g. `web-x#slides`). The SPA's
    // `session/open` subscription is keyed only on the base SessionKey;
    // `handle_turn_start` folds the topic into `params.session_id` but
    // never re-subscribes. Every published frame therefore reached zero
    // live subscribers (fleet evidence: bot mini2 session
    // web-1779812289419-27ht74, seq=92 file/attached landed on
    // `web-…#slides` bucket with zero subscribers).
    //
    // The fix strips the topic at the emit site so the envelope lands
    // on the base bucket where the SPA actually listens. The companion
    // 35a68cb9 topic-scope-filter exemption is correct on its own but
    // wasn't the load-bearing fix because there were no subscribers to
    // filter at all.
    // ─────────────────────────────────────────────────────────────────────

    /// P0-A unit invariant — when the caller passes a topic-suffixed
    /// session key, every `file/attached` envelope must publish on the
    /// BASE (no-topic) SessionKey so the SPA's session/open
    /// subscription actually receives it.
    #[test]
    fn emit_files_attached_from_background_publishes_on_base_session_key_not_topic_suffix() {
        let ledger = Arc::new(UiProtocolLedger::new(64));
        // Caller-side key is topic-suffixed (mirrors what
        // `handle_turn_start` produces after folding `params.topic` into
        // `params.session_id`).
        let topic_session = SessionKey::with_topic("api", "p0a-base-key", "slides");
        let base_session = SessionKey::new("api", "p0a-base-key");
        let turn_id = TurnId::new();

        // Subscribe on the BASE key (this is what the SPA does via
        // `session/open` — see ui_protocol.rs:7479).
        let mut base_subscriber = ledger.subscribe(&base_session);
        // Subscribe on the topic-suffixed key too, so we can prove the
        // envelope is NOT being published there (pre-fix behaviour).
        let mut topic_subscriber = ledger.subscribe(&topic_session);

        emit_files_attached_from_background(
            &ledger,
            &topic_session,
            &turn_id,
            &["/tmp/deck.pptx".to_string()],
            &[],
            Some("tc-p0a".into()),
        );

        // Base subscriber MUST receive the frame.
        let event = base_subscriber
            .try_recv()
            .expect("base SessionKey subscriber must receive file/attached");
        match &event.event {
            crate::api::ui_protocol_ledger::UiProtocolLedgerEvent::Notification(
                UiNotification::FileAttached(payload),
            ) => {
                assert_eq!(
                    payload.session_id, base_session,
                    "embedded session_id must be the base form (no topic suffix)",
                );
                assert_eq!(payload.path, "/tmp/deck.pptx");
            }
            other => panic!("expected FileAttached, got {other:?}"),
        }

        // Topic-suffixed subscriber MUST NOT receive the frame — that's
        // the bucket that was previously fanning out to zero subscribers
        // in production.
        assert!(
            topic_subscriber.try_recv().is_err(),
            "topic-suffixed broadcast bucket must NOT receive the envelope; \
             publishing there is the bug — the SPA never subscribes to it",
        );
    }

    /// P0-A integration — a subscriber that opened on the base
    /// SessionKey (the SPA's actual session/open shape) receives a
    /// `file/attached` envelope emitted by a spawn_only completion whose
    /// `session_id` carries the topic suffix. Pre-fix this dropped the
    /// frame on the floor.
    #[test]
    fn base_session_subscriber_receives_file_attached_when_emit_carries_topic_suffix() {
        let ledger = Arc::new(UiProtocolLedger::new(64));
        let base_session = SessionKey::new("api", "p0a-integration");
        // Spawn_only completion's session_id carries the topic
        // (production shape after handle_turn_start folds params.topic).
        let topic_session = SessionKey::with_topic("api", "p0a-integration", "slides");
        let turn_id = TurnId::new();

        let mut subscriber = ledger.subscribe(&base_session);

        // Production shape: `mofa_slides` Satisfied path with one PPTX
        // artefact, emitted onto a topic-suffixed session_id.
        let deck = "/Users/cloud/.octos/profiles/dspfac/data/slides/p0a/output/deck.pptx";
        emit_files_attached_from_background(
            &ledger,
            &topic_session,
            &turn_id,
            &[deck.to_string()],
            &[],
            Some("tc-p0a-integration".into()),
        );

        let event = subscriber.try_recv().expect(
            "base SessionKey subscriber must receive file/attached \
                     even when the emit site carries a topic-suffixed session_id",
        );
        let rpc = event
            .event
            .clone()
            .into_rpc_notification()
            .expect("serializes");
        assert_eq!(rpc.method, methods::FILE_ATTACHED);
        if let crate::api::ui_protocol_ledger::UiProtocolLedgerEvent::Notification(
            UiNotification::FileAttached(payload),
        ) = &event.event
        {
            assert_eq!(payload.path, deck);
            // Embedded session_id is also rewritten to the base form so
            // the wire shape is internally consistent (clients keying
            // off `event.session_id` see the bare form, matching the
            // bucket the frame actually rode).
            assert_eq!(payload.session_id, base_session);
        } else {
            panic!("expected FileAttached, got {:?}", event.event);
        }
    }

    /// P0-A defensive — when the emit site already passes the BASE
    /// SessionKey (no topic suffix), behaviour is identical to pre-fix.
    /// Pins that the strip is idempotent and doesn't accidentally regress
    /// the bare-session case the existing helper tests cover.
    #[test]
    fn emit_files_attached_from_background_idempotent_for_base_session_input() {
        let ledger = Arc::new(UiProtocolLedger::new(64));
        let base_session = SessionKey::new("api", "p0a-idempotent");
        let turn_id = TurnId::new();
        let mut subscriber = ledger.subscribe(&base_session);

        emit_files_attached_from_background(
            &ledger,
            &base_session,
            &turn_id,
            &["/tmp/report.md".to_string()],
            &[],
            None,
        );

        let event = subscriber.try_recv().expect("base subscriber receives");
        if let crate::api::ui_protocol_ledger::UiProtocolLedgerEvent::Notification(
            UiNotification::FileAttached(payload),
        ) = &event.event
        {
            assert_eq!(payload.session_id, base_session);
            assert_eq!(payload.path, "/tmp/report.md");
            assert!(
                payload.topic.is_none(),
                "bare session_id emit must preserve the no-topic shape",
            );
        } else {
            panic!("expected FileAttached");
        }
    }

    /// #1336 round-2 (codex BLOCK): the original P0-A fix (`a303991c`)
    /// stripped the topic suffix from `session_id` BEFORE building the
    /// `FileAttachedEvent`, so the helper saw a bare session and the
    /// emitted event ended up with `topic: None`. After the
    /// `ledger_event_matches_topic_scope` exemption was removed in
    /// favour of consulting the explicit `event.topic` field, an event
    /// with `topic: None` is silently dropped by a topic-scoped
    /// subscriber — re-introducing the exact failure mode P0-A
    /// originally closed. The append-time safety net
    /// (`stamp_topic_from_session`) can't recover because `session_id`
    /// has already been stripped, so its `session_id.topic()` returns
    /// `None` and it bails out at the early-return guard.
    ///
    /// The fix preserves topic on the event itself BEFORE the strip:
    /// the caller captures `session_id.topic()` and passes it
    /// separately into `emit_file_attached`, which sets the event's
    /// `topic` field explicitly. The base SessionKey routing
    /// (subscribers on the base key) is unchanged.
    ///
    /// Pre-fix: this assertion fails because `event.topic` is `None`.
    /// Post-fix: `event.topic` is `Some("slides")` so the topic-scoped
    /// classifier (which reads `event.topic()` first) accepts the
    /// event.
    #[test]
    fn emit_files_attached_from_background_preserves_topic_on_event_when_session_id_stripped() {
        let ledger = Arc::new(UiProtocolLedger::new(64));
        let base_session = SessionKey::new("api", "p0a-round2");
        // Caller-side session_id carries the `#slides` topic suffix —
        // mirrors the production shape that surfaced the bug.
        let topic_session = SessionKey::with_topic("api", "p0a-round2", "slides");
        let turn_id = TurnId::new();
        let mut subscriber = ledger.subscribe(&base_session);

        emit_files_attached_from_background(
            &ledger,
            &topic_session,
            &turn_id,
            &["/tmp/deck.pptx".to_string()],
            &[],
            Some("tc-round2".into()),
        );

        let event = subscriber
            .try_recv()
            .expect("base subscriber receives file/attached");
        let (payload, notification_topic) = match &event.event {
            crate::api::ui_protocol_ledger::UiProtocolLedgerEvent::Notification(
                notification @ UiNotification::FileAttached(payload),
            ) => (payload.clone(), notification.topic().map(ToOwned::to_owned)),
            other => panic!("expected FileAttached, got {other:?}"),
        };

        // Routing invariant (P0-A): the embedded session_id is the
        // BASE form so the SPA's `session/open` subscription receives it.
        assert_eq!(
            payload.session_id, base_session,
            "embedded session_id must be the base form (no topic suffix)",
        );

        // Topic invariant (#1336 round-2 BLOCKER): the event's `topic`
        // field must be populated from the ORIGINAL topic-suffixed
        // session_id BEFORE the strip — otherwise the topic-scoped
        // classifier drops the event silently.
        assert_eq!(
            payload.topic.as_deref(),
            Some("slides"),
            "event.topic must be preserved from the original session_id; \
             the strip site captures it BEFORE rebuilding the base SessionKey",
        );

        // `UiNotification::topic()` (the helper the classifier consults)
        // must surface the explicit field so a topic-scoped subscriber
        // accepts the event. Pre-fix this returned None because the
        // base-stripped `session_id` had no `#topic` suffix to fall
        // back on either — both the explicit field and the suffix
        // fallback were empty, and the classifier dropped the event.
        assert_eq!(
            notification_topic.as_deref(),
            Some("slides"),
            "UiNotification::topic() (read by the topic-scope classifier) \
             must return the explicit event.topic so the topic-scoped \
             subscriber routes the envelope correctly",
        );
    }
}
