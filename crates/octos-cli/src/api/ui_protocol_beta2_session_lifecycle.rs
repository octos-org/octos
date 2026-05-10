//! M9-β-2 — emit `session/closed` and `session/title-updated` onto the
//! M9 ledger when the corresponding REST endpoint is called.
//!
//! ## Why this exists
//!
//! The α-3 bridge note (`ui_protocol_alpha3_bridge.rs`) deferred these
//! two lifecycle envelopes:
//!
//! | wire `kind`            | source                                                | status pre-β-2 |
//! |------------------------|-------------------------------------------------------|----------------|
//! | `session/closed`       | implicit on `DELETE /api/sessions/:id`                | not emitted    |
//! | `session/title-updated`| implicit on `PATCH /api/sessions/:id/title`           | not emitted    |
//!
//! Without these, web clients that render a session sidebar must poll
//! `GET /api/sessions` to learn that another tab deleted a row or that
//! the auto-titler renamed one. The whole point of the M9 WS path is
//! to remove polling — these helpers close that gap.
//!
//! ## Idempotency / failure mode
//!
//! Both helpers append best-effort. The REST handler still returns its
//! HTTP response regardless of ledger outcome — a ledger append failure
//! does not affect the REST contract. A subscriber that misses the
//! envelope (no live forwarder, dropped buffer slot) eventually sees
//! the row through replay on the next `session/open` for that session,
//! or through the durable session list endpoint.
//!
//! ## Out of scope
//!
//! - Direct WS-RPC `session/close` command (no client wants one today;
//!   REST DELETE is canonical).
//! - Auto-titler emission. The auto-titler writes through
//!   `SessionManager::update_title` deep inside `octos-bus`. Wiring an
//!   observer there would mean threading the API ledger across crate
//!   boundaries; β-2 hooks the REST PATCH path only and lets the
//!   auto-titler's same-key write surface as a follow-up REST PATCH
//!   from the title generator's caller. That covers the manual-rename
//!   case immediately. The pure-bus auto-title path remains a
//!   client-side polling fallback until a future `MessageCommitObserver`
//!   gets a `TitleUpdatedObserver` peer (tracked separately).

use std::sync::Arc;

use chrono::Utc;
use octos_core::SessionKey;
use octos_core::ui_protocol::{SessionClosedEvent, SessionTitleUpdatedEvent, UiNotification};

use super::ui_protocol_ledger::UiProtocolLedger;

/// Append a `session/closed` notification to the ledger.
///
/// Callers: [`super::handlers::delete_session`].
///
/// `reason` is a free-form discriminator. The current canonical value
/// is `"deleted"`; future producers can use `"expired"`, `"forked"`, etc.
/// Clients should treat unknown reasons as opaque.
pub(super) fn emit_session_closed(
    ledger: &Arc<UiProtocolLedger>,
    session_id: &SessionKey,
    reason: &str,
) {
    let notification = UiNotification::SessionClosed(SessionClosedEvent {
        session_id: session_id.clone(),
        reason: Some(reason.to_string()),
        timestamp: Utc::now(),
        cursor: None,
    });
    let _ = ledger.append_notification(notification);
}

/// Append a `session/title-updated` notification to the ledger.
///
/// Callers: [`super::handlers::update_session_title`] (manual rename
/// via REST PATCH). Auto-titler emission is intentionally not covered
/// here — see module-level docs.
pub(super) fn emit_session_title_updated(
    ledger: &Arc<UiProtocolLedger>,
    session_id: &SessionKey,
    title: &str,
    reason: &str,
) {
    let notification = UiNotification::SessionTitleUpdated(SessionTitleUpdatedEvent {
        session_id: session_id.clone(),
        title: title.to_string(),
        reason: Some(reason.to_string()),
        timestamp: Utc::now(),
        cursor: None,
    });
    let _ = ledger.append_notification(notification);
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_core::ui_protocol::methods;

    /// β-2 acceptance gate (A): emitting session/closed lands the
    /// envelope on the broadcast for the right session, with the right
    /// method name, reason, and ledger-stamped cursor.
    #[test]
    fn should_emit_session_closed_with_method_name_and_cursor() {
        let ledger = Arc::new(UiProtocolLedger::new(64));
        let session_id = SessionKey::new("api", "beta2-closed");
        let mut subscriber = ledger.subscribe(&session_id);

        emit_session_closed(&ledger, &session_id, "deleted");

        let event = subscriber.try_recv().expect("ledger broadcast");
        let notification = match event.event {
            crate::api::ui_protocol_ledger::UiProtocolLedgerEvent::Notification(n) => n,
            other => panic!("expected Notification, got {other:?}"),
        };
        assert_eq!(notification.method(), methods::SESSION_CLOSED);
        match notification {
            UiNotification::SessionClosed(closed) => {
                assert_eq!(closed.session_id, session_id);
                assert_eq!(closed.reason.as_deref(), Some("deleted"));
                let cursor = closed
                    .cursor
                    .as_ref()
                    .expect("ledger must stamp cursor onto SessionClosedEvent");
                assert_eq!(cursor.stream, session_id.0);
                assert!(cursor.seq > 0);
            }
            other => panic!("expected SessionClosed, got {other:?}"),
        }
        assert!(subscriber.try_recv().is_err());
    }

    /// β-2 acceptance gate (B): emitting session/title-updated carries
    /// the new title and reason, lands on the right session, and gets
    /// a ledger cursor stamp.
    #[test]
    fn should_emit_session_title_updated_with_title_and_reason() {
        let ledger = Arc::new(UiProtocolLedger::new(64));
        let session_id = SessionKey::new("api", "beta2-titled");
        let mut subscriber = ledger.subscribe(&session_id);

        emit_session_title_updated(&ledger, &session_id, "My new title", "manual");

        let event = subscriber.try_recv().expect("ledger broadcast");
        let notification = match event.event {
            crate::api::ui_protocol_ledger::UiProtocolLedgerEvent::Notification(n) => n,
            other => panic!("expected Notification, got {other:?}"),
        };
        assert_eq!(notification.method(), methods::SESSION_TITLE_UPDATED);
        match notification {
            UiNotification::SessionTitleUpdated(updated) => {
                assert_eq!(updated.session_id, session_id);
                assert_eq!(updated.title, "My new title");
                assert_eq!(updated.reason.as_deref(), Some("manual"));
                let cursor = updated
                    .cursor
                    .as_ref()
                    .expect("ledger must stamp cursor onto SessionTitleUpdatedEvent");
                assert_eq!(cursor.stream, session_id.0);
                assert!(cursor.seq > 0);
            }
            other => panic!("expected SessionTitleUpdated, got {other:?}"),
        }
    }

    /// β-2 acceptance gate (C): the bridge's emits route to the caller-
    /// supplied SessionKey only, never cross-deliver to other sessions
    /// (mirrors the α-3 bridge isolation invariant).
    #[test]
    fn should_route_lifecycle_to_caller_session_only() {
        let ledger = Arc::new(UiProtocolLedger::new(64));
        let a = SessionKey::new("api", "beta2-iso-a");
        let b = SessionKey::new("api", "beta2-iso-b");
        let mut sub_a = ledger.subscribe(&a);
        let mut sub_b = ledger.subscribe(&b);

        emit_session_closed(&ledger, &a, "deleted");
        emit_session_title_updated(&ledger, &a, "renamed-a", "manual");

        assert!(sub_a.try_recv().is_ok());
        assert!(sub_a.try_recv().is_ok());
        assert!(sub_a.try_recv().is_err());
        assert!(
            sub_b.try_recv().is_err(),
            "lifecycle envelopes must NOT cross-deliver to other session subscribers"
        );
    }

    /// β-2 acceptance gate (D): both methods serialize with the v1
    /// wire method names. A WS reducer routing by method name would
    /// silently drop frames otherwise.
    #[test]
    fn should_serialize_with_v1_method_names() {
        let ledger = Arc::new(UiProtocolLedger::new(64));
        let session_id = SessionKey::new("api", "beta2-method-names");
        let mut subscriber = ledger.subscribe(&session_id);

        emit_session_closed(&ledger, &session_id, "deleted");
        emit_session_title_updated(&ledger, &session_id, "t", "manual");

        let closed = subscriber.try_recv().unwrap();
        let closed_rpc = closed
            .event
            .clone()
            .into_rpc_notification()
            .expect("session/closed serializes");
        assert_eq!(closed_rpc.method, "session/closed");

        let titled = subscriber.try_recv().unwrap();
        let titled_rpc = titled
            .event
            .clone()
            .into_rpc_notification()
            .expect("session/title-updated serializes");
        assert_eq!(titled_rpc.method, "session/title-updated");
    }
}
