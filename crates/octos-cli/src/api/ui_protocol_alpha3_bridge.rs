//! M9-α-3 — bridge SSE-driven session lifecycle events onto the M9
//! WebSocket UI Protocol path.
//!
//! Per the M9-α (Sole Transport) ADR (`docs/M9-ALPHA-SOLE-TRANSPORT-ADR.md`)
//! the WebSocket transport is migrating to be the sole chat transport.
//! This module is the α-3 phase: while SSE is still alive (deletion
//! lands in α-5/α-6 atomically with the web bundle), every session
//! lifecycle event the SSE chat path emits during a
//! `POST /api/chat?stream=true` turn must ALSO be appended to the M9
//! ledger so any concurrently-connected WebSocket subscriber for the
//! same `SessionKey` sees it through the live broadcast
//! (`UiProtocolLedger::subscribe`).
//!
//! **Survey of session lifecycle SSE events** (issue #832, audit-lock
//! #845). The SSE chat path emits the following session-scoped frames:
//!
//! | SSE wire `type`     | source                                                       | already on WS? |
//! |---------------------|--------------------------------------------------------------|----------------|
//! | `session_result`    | `handlers.rs::chat_streaming` (user msg, line ~695)         | yes — via `MessageCommitObserver` (UPCR-2026-012) |
//! | `session_result`    | `octos_bus::api_channel::build_session_result_event` (asst) | yes — via `MessageCommitObserver` (UPCR-2026-012) |
//! | `done` (turn end)   | `handlers.rs::chat_streaming` (line ~723)                   | **no** — added by α-3 as `turn/completed.v1` |
//! | (no `turn_started`) | implicit at handler entry — no SSE frame emitted             | **no** — added by α-3 as `turn/started.v1` |
//! | (no `session_open`) | implicit on subscribe — no SSE frame emitted                 | n/a — `session/open` is a WS RPC, has no SSE counterpart |
//! | (no `session_close`)| implicit on `delete_session` — no SSE frame emitted          | n/a — REST DELETE, has no SSE counterpart |
//! | (no `session_title`)| implicit on `update_session_title` — no SSE frame emitted    | n/a — REST PATCH, has no SSE counterpart |
//!
//! The two SSE `session_result` variants are durable per-row commits;
//! `MessageCommitObserver` (installed in `ui_protocol::install_message_commit_observer`)
//! mirrors EVERY successful `add_message_with_seq` commit to the ledger
//! as a `message/persisted.v1` notification — so a WS subscriber for
//! the same session already receives a coherent durable view of those
//! rows. The α-3 bridge does NOT append a second envelope for those
//! rows; doing so would emit duplicate persistence confirmations on the
//! wire. The unit test
//! `should_session_result_already_lands_on_ledger_via_observer` is the
//! regression guard for this invariant.
//!
//! What α-3 DOES add is the per-turn lifecycle pair (`turn/started.v1`
//! and `turn/completed.v1`) that the SSE chat path implicitly carries
//! via the open/close of its `Sse` stream. Without these envelopes a WS
//! subscriber that connects mid-turn (a) cannot observe the turn
//! starting and (b) does not see a turn-end signal for SSE-driven
//! turns. Per UPCR-2026-014 the wire shape is the existing
//! `TurnStartedEvent` / `TurnCompletedEvent`, so this is a
//! capability-additive bridge, not a spec change.
//!
//! **Coexistence invariants**:
//! - SSE delivery is unchanged. Every existing SSE wire frame is still
//!   sent via the original `tx` channel.
//! - Ledger appends are best-effort. A failure does not affect the SSE
//!   path or the agent loop.
//! - The web reducer (`MessageStore`) routes by either path and
//!   collapses duplicates by their stable identity (turn_id +
//!   session_id), so a client connected to both transports sees one
//!   logical lifecycle event per turn. That is the explicit dedup
//!   contract for the α-3 coexistence period.
//!
//! Out of scope for α-3 (deferred to α-7 / γ-3):
//! - `session/open`, `session/close`, `session/title-updated` are WS-
//!   exclusive surfaces today and need spec work before becoming
//!   bridgeable.
//! - Web client opt-in to consume the new envelopes — the web change
//!   lands in α-7 alongside the SSE deletion. Until then the bridge is
//!   purely additive on the wire.
//!
//! When α-5/α-6 land and SSE is deleted, the bridge calls become the
//! only path emitting these envelopes — they remain correct because
//! they always hit the ledger first regardless of SSE state.

use std::sync::Arc;

use chrono::Utc;
use octos_core::SessionKey;
use octos_core::ui_protocol::{TurnCompletedEvent, TurnId, TurnStartedEvent, UiNotification};

use super::ui_protocol_ledger::UiProtocolLedger;

/// Append a `turn/started.v1` notification to the ledger, mirroring the
/// implicit "SSE stream opened" lifecycle the chat_streaming path
/// previously had no WS counterpart for.
///
/// Idempotency: callers should invoke this exactly once per chat turn,
/// after the turn's `TurnId` is generated and before the agent loop
/// starts emitting per-iteration events. Calling twice for the same
/// turn would emit two `turn/started` envelopes, which violates the
/// per-turn ordering contract every WS reducer assumes.
///
/// Failure mode: ledger append failures are logged inside the ledger
/// and do not propagate. SSE delivery continues unaffected — that is
/// the explicit α-3 coexistence invariant.
pub(super) fn emit_turn_started(
    ledger: &Arc<UiProtocolLedger>,
    session_id: &SessionKey,
    turn_id: &TurnId,
) {
    let notification = UiNotification::TurnStarted(TurnStartedEvent {
        session_id: session_id.clone(),
        turn_id: turn_id.clone(),
        timestamp: Utc::now(),
    });
    let _ = ledger.append_notification(notification);
}

/// Append a `turn/completed.v1` notification to the ledger when the SSE
/// chat path is about to send its terminal `done` (or `error`) frame.
///
/// The ledger overwrites the `cursor` field with the assigned ledger
/// seq via [`UiProtocolLedgerEvent::with_cursor`] (see
/// `ui_protocol_ledger.rs`), so the placeholder `None` here is the
/// canonical caller-side input — every other producer in the file does
/// the same.
///
/// Failure mode: same as [`emit_turn_started`] — ledger errors are
/// non-fatal for SSE.
pub(super) fn emit_turn_completed(
    ledger: &Arc<UiProtocolLedger>,
    session_id: &SessionKey,
    turn_id: &TurnId,
) {
    let notification = UiNotification::TurnCompleted(TurnCompletedEvent {
        session_id: session_id.clone(),
        turn_id: turn_id.clone(),
        cursor: None,
    });
    let _ = ledger.append_notification(notification);
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_core::ui_protocol::{
        MessagePersistedEvent, MessagePersistedSource, UiCursor, methods,
    };

    /// α-3 acceptance gate: emitting a turn lifecycle pair lands BOTH
    /// envelopes on the M9 ledger broadcast for the right session,
    /// in the right order, with the right method names.
    #[test]
    fn should_emit_turn_started_then_completed_in_order() {
        let ledger = Arc::new(UiProtocolLedger::new(64));
        let session_id = SessionKey::new("api", "alpha3-lifecycle");
        let turn_id = TurnId::new();

        let mut subscriber = ledger.subscribe(&session_id);

        emit_turn_started(&ledger, &session_id, &turn_id);
        emit_turn_completed(&ledger, &session_id, &turn_id);

        // First event: turn/started, with the same turn_id we emitted.
        let started = subscriber
            .try_recv()
            .expect("ledger must broadcast turn/started");
        let started_method = match &started.event {
            crate::api::ui_protocol_ledger::UiProtocolLedgerEvent::Notification(n) => n.method(),
            other => panic!("expected Notification, got {other:?}"),
        };
        assert_eq!(started_method, methods::TURN_STARTED);
        match &started.event {
            crate::api::ui_protocol_ledger::UiProtocolLedgerEvent::Notification(
                UiNotification::TurnStarted(payload),
            ) => {
                assert_eq!(payload.session_id, session_id);
                assert_eq!(payload.turn_id, turn_id);
            }
            other => panic!("expected TurnStarted notification, got {other:?}"),
        }

        // Second event: turn/completed, same turn_id, ledger-stamped cursor.
        let completed = subscriber
            .try_recv()
            .expect("ledger must broadcast turn/completed");
        let completed_method = match &completed.event {
            crate::api::ui_protocol_ledger::UiProtocolLedgerEvent::Notification(n) => n.method(),
            other => panic!("expected Notification, got {other:?}"),
        };
        assert_eq!(completed_method, methods::TURN_COMPLETED);
        match &completed.event {
            crate::api::ui_protocol_ledger::UiProtocolLedgerEvent::Notification(
                UiNotification::TurnCompleted(payload),
            ) => {
                assert_eq!(payload.session_id, session_id);
                assert_eq!(payload.turn_id, turn_id);
                // Ledger stamps the cursor onto TurnCompleted per
                // `with_cursor` in `ui_protocol_ledger.rs`. The cursor
                // must therefore be Some after the append.
                let cursor = payload
                    .cursor
                    .as_ref()
                    .expect("ledger must stamp cursor onto TurnCompletedEvent per UPCR-2026-007");
                assert_eq!(cursor.stream, session_id.0);
                assert!(
                    cursor.seq > 0,
                    "cursor seq must be assigned, got {cursor:?}"
                );
            }
            other => panic!("expected TurnCompleted notification, got {other:?}"),
        }

        // Coexistence invariant: each lifecycle envelope emits exactly once.
        assert!(
            subscriber.try_recv().is_err(),
            "lifecycle bridge must emit exactly one envelope per call"
        );
    }

    /// α-3 acceptance gate (B): the rpc method names that ride on the WS
    /// wire match the v1alpha1 spec. Without this, a WS client routing by
    /// method name (`turn/started`, `turn/completed`) would silently drop
    /// the bridged frames.
    #[test]
    fn should_serialize_lifecycle_with_v1_method_names() {
        let ledger = Arc::new(UiProtocolLedger::new(64));
        let session_id = SessionKey::new("api", "alpha3-method-names");
        let turn_id = TurnId::new();

        let mut subscriber = ledger.subscribe(&session_id);

        emit_turn_started(&ledger, &session_id, &turn_id);
        emit_turn_completed(&ledger, &session_id, &turn_id);

        let started = subscriber.try_recv().unwrap();
        let started_rpc = started
            .event
            .clone()
            .into_rpc_notification()
            .expect("turn/started serializes");
        assert_eq!(started_rpc.method, "turn/started");

        let completed = subscriber.try_recv().unwrap();
        let completed_rpc = completed
            .event
            .clone()
            .into_rpc_notification()
            .expect("turn/completed serializes");
        assert_eq!(completed_rpc.method, "turn/completed");
    }

    /// α-3 acceptance gate (C): the bridge must NOT emit a duplicate
    /// `message/persisted` envelope for `session_result` SSE events,
    /// because `MessageCommitObserver` already handles those. We assert
    /// the existing observer behaviour by appending a `MessagePersisted`
    /// directly (mirroring what the observer does) and confirming the
    /// bridge's lifecycle calls do NOT add a second one for the same
    /// row.
    ///
    /// This is a regression guard against a future refactor that
    /// "helpfully" mirrors session_result through the bridge — which
    /// would put two persistence confirmations on the wire for the
    /// same row.
    #[test]
    fn should_session_result_already_lands_on_ledger_via_observer() {
        use chrono::DateTime;

        let ledger = Arc::new(UiProtocolLedger::new(64));
        let session_id = SessionKey::new("api", "alpha3-observer-coexistence");
        let turn_id = TurnId::new();

        let mut subscriber = ledger.subscribe(&session_id);

        // Step 1: simulate `MessageCommitObserver` firing for a
        // user-message commit, exactly as `install_message_commit_observer`
        // does at `ui_protocol.rs:920-973`.
        let observer_event = UiNotification::MessagePersisted(MessagePersistedEvent {
            session_id: session_id.clone(),
            turn_id: Some(turn_id.clone()),
            thread_id: Some("cmid-alpha-3".into()),
            seq: 42,
            role: "user".into(),
            message_id: format!("{}:42:0", session_id.0),
            client_message_id: Some("cmid-alpha-3".into()),
            source: MessagePersistedSource::User,
            cursor: UiCursor {
                stream: session_id.0.clone(),
                seq: 0, // placeholder; ledger overwrites
            },
            persisted_at: DateTime::from_timestamp(0, 0).unwrap(),
            media: vec![],
        });
        let _ = ledger.append_notification(observer_event);

        // Step 2: the bridge fires its turn lifecycle pair — these are
        // ADDITIVE, not duplicates of the observer's persistence event.
        emit_turn_started(&ledger, &session_id, &turn_id);
        emit_turn_completed(&ledger, &session_id, &turn_id);

        // Drain the broadcast and confirm we see exactly:
        //   1. message/persisted (from observer simulation)
        //   2. turn/started      (from bridge)
        //   3. turn/completed    (from bridge)
        // and NO duplicate message/persisted from the bridge.
        let mut method_sequence = Vec::new();
        while let Ok(event) = subscriber.try_recv() {
            if let crate::api::ui_protocol_ledger::UiProtocolLedgerEvent::Notification(n) =
                &event.event
            {
                method_sequence.push(n.method().to_string());
            }
        }
        assert_eq!(
            method_sequence,
            vec![
                methods::MESSAGE_PERSISTED.to_string(),
                methods::TURN_STARTED.to_string(),
                methods::TURN_COMPLETED.to_string(),
            ],
            "bridge must NOT duplicate message/persisted; observer is the sole source"
        );
    }

    /// α-3 acceptance gate (D): the bridge's emits route to the
    /// SessionKey it was given, not to a different session. Without
    /// this, a multi-session process (the standard `octos serve`
    /// shape) would cross-deliver lifecycle envelopes between
    /// concurrently-active turns.
    #[test]
    fn should_route_lifecycle_to_caller_session_only() {
        let ledger = Arc::new(UiProtocolLedger::new(64));
        let session_a = SessionKey::new("api", "alpha3-iso-A");
        let session_b = SessionKey::new("api", "alpha3-iso-B");
        let turn_id = TurnId::new();

        let mut sub_a = ledger.subscribe(&session_a);
        let mut sub_b = ledger.subscribe(&session_b);

        // Fire on session_a only.
        emit_turn_started(&ledger, &session_a, &turn_id);
        emit_turn_completed(&ledger, &session_a, &turn_id);

        // Session A receives both envelopes.
        assert!(sub_a.try_recv().is_ok());
        assert!(sub_a.try_recv().is_ok());
        assert!(sub_a.try_recv().is_err());

        // Session B receives nothing — no cross-delivery.
        assert!(
            sub_b.try_recv().is_err(),
            "lifecycle envelopes must NOT cross-deliver to other session subscribers"
        );
    }
}
