//! M9-α-9 — bridge SSE-only events that α-7 e2e rewrite (PR #851)
//! flagged as still-fixme'd onto the M9 WebSocket UI Protocol path.
//!
//! Per the M9-α (Sole Transport) ADR (`docs/M9-ALPHA-SOLE-TRANSPORT-ADR.md`)
//! the WebSocket transport is migrating to be the sole chat transport.
//! This module is the α-9 phase: while SSE is still alive (deletion lands
//! in α-5/α-6 atomically with the web bundle), 5 events α-7 surfaced as
//! SSE-only must ALSO land on the M9 ledger so any concurrently-connected
//! WebSocket subscriber for the same `SessionKey` receives them. Without
//! these bridges the corresponding α-7 specs cannot be un-fixme'd.
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
//! When α-5/α-6 land and SSE is deleted, the bridge calls become the
//! only path emitting these envelopes — they remain correct because
//! they always hit the ledger first regardless of SSE state.

use std::sync::Arc;

use chrono::Utc;
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
/// `tool_call_id` is optional because not every file-emission path
/// runs inside a tool execution (rare; reserved for background-result
/// futures). `mime` is also optional — clients fall back to extension
/// sniffing when absent.
pub(super) fn emit_file_attached(
    ledger: &Arc<UiProtocolLedger>,
    session_id: &SessionKey,
    turn_id: &TurnId,
    path: String,
    tool_call_id: Option<String>,
    mime: Option<String>,
) {
    let notification = UiNotification::FileAttached(FileAttachedEvent {
        session_id: session_id.clone(),
        turn_id: turn_id.clone(),
        path,
        tool_call_id,
        mime,
    });
    let _ = ledger.append_notification(notification);
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
        emit_file_attached(&ledger, &session_a, &turn_id, "/tmp/x".into(), None, None);
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
}
