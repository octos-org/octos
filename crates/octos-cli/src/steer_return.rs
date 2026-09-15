//! task-return-unconsumed-steer-inputs: build the `turn/steer_dropped`
//! notification that hands accepted-but-undrained `turn/steer` inputs back to
//! the client at turn end. Kept outside the `api` feature so its shape is
//! verifiable with a plain `cargo test -p octos-cli`; the `api` side
//! (`settle_leftover_steers`, invoked from the single terminal gate
//! `transition_to_terminal_settling_steers`) drains the buffer, calls this
//! and sends it — always BEFORE the terminal frame (`event.turn_steer_dropped.v1`).

use octos_core::SessionKey;
use octos_core::ui_protocol::{TurnId, TurnSteerDroppedEvent, UiNotification};

/// `reason` when the turn was interrupted by the client.
pub(crate) const REASON_INTERRUPTED: &str = "interrupted";
/// `reason` when the turn ended on its own (EndTurn / error) with input pending.
pub(crate) const REASON_TURN_ENDED: &str = "turn_ended";

/// One `turn/steer_dropped` for the given leftovers, in buffer order. `None`
/// when there is nothing to return — an empty return frame is never emitted.
pub(crate) fn leftover_steer_notification(
    session_id: &SessionKey,
    turn_id: &TurnId,
    leftovers: Vec<String>,
    interrupt_observed: bool,
) -> Option<UiNotification> {
    if leftovers.is_empty() {
        return None;
    }
    let reason = if interrupt_observed {
        REASON_INTERRUPTED
    } else {
        REASON_TURN_ENDED
    };
    Some(UiNotification::TurnSteerDropped(TurnSteerDroppedEvent {
        session_id: session_id.clone(),
        topic: None,
        turn_id: turn_id.clone(),
        inputs: leftovers,
        reason: reason.to_owned(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_core::ui_protocol::{TurnCompletedEvent, TurnErrorEvent, methods};

    fn ids() -> (SessionKey, TurnId) {
        (SessionKey("local:steer".into()), TurnId::new())
    }

    #[test]
    fn leftover_steer_notification_preserves_order_and_labels_interrupted() {
        let (session_id, turn_id) = ids();
        let notification = leftover_steer_notification(
            &session_id,
            &turn_id,
            vec!["first steer".into(), "second steer".into()],
            true,
        )
        .expect("two leftovers produce one notification");
        assert_eq!(notification.method(), methods::TURN_STEER_DROPPED);
        assert_eq!(notification.method(), "turn/steer_dropped");
        let UiNotification::TurnSteerDropped(event) = notification else {
            panic!("expected TurnSteerDropped");
        };
        assert_eq!(event.session_id, session_id);
        assert_eq!(event.turn_id, turn_id);
        assert_eq!(event.inputs, vec!["first steer", "second steer"]);
        assert_eq!(event.reason, REASON_INTERRUPTED);
        assert_eq!(event.reason, "interrupted");
    }

    #[test]
    fn leftover_steer_notification_labels_turn_ended_without_interrupt() {
        let (session_id, turn_id) = ids();
        let UiNotification::TurnSteerDropped(event) =
            leftover_steer_notification(&session_id, &turn_id, vec!["late".into()], false)
                .expect("one leftover produces one notification")
        else {
            panic!("expected TurnSteerDropped");
        };
        assert_eq!(event.reason, "turn_ended");
        assert_eq!(event.inputs, vec!["late"]);
    }

    #[test]
    fn no_leftover_steers_produce_no_notification() {
        let (session_id, turn_id) = ids();
        assert!(leftover_steer_notification(&session_id, &turn_id, vec![], true).is_none());
        assert!(leftover_steer_notification(&session_id, &turn_id, vec![], false).is_none());

        // The terminal payloads keep their exact field sets — the return is a
        // separate frame, never folded into turn/error or turn/completed.
        let error = UiNotification::TurnError(TurnErrorEvent {
            session_id: session_id.clone(),
            topic: None,
            turn_id: turn_id.clone(),
            code: "interrupted".into(),
            message: "turn interrupted by client".into(),
            token_usage: None,
            partial_result: None,
        })
        .into_rpc_notification()
        .expect("serialize turn/error");
        let error_keys: Vec<&str> = error
            .params
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(error_keys, vec!["session_id", "turn_id", "code", "message"]);

        let completed = UiNotification::TurnCompleted(TurnCompletedEvent {
            session_id,
            topic: None,
            turn_id,
            cursor: None,
            tokens_in: None,
            tokens_out: None,
            session_result: None,
        })
        .into_rpc_notification()
        .expect("serialize turn/completed");
        let completed_keys: Vec<&str> = completed
            .params
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(completed_keys, vec!["session_id", "turn_id"]);
    }
}

#[cfg(test)]
mod gate_tests {
    /// Structural guard, feature-independent: `transition_to_terminal(` is
    /// invoked ONLY inside `transition_to_terminal_settling_steers` — every
    /// terminal outlet (live emit, connection-close abort, early bail-outs,
    /// fixtures) must go through the settling gate, or the
    /// `event.turn_steer_dropped.v1` order promise breaks silently.
    #[test]
    fn every_terminal_outlet_goes_through_the_settling_gate() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/api/ui_protocol_transport.rs");
        let text = std::fs::read_to_string(&path).expect("read ui_protocol_transport.rs");
        let mut current_fn = String::new();
        let mut offenders = Vec::new();
        for (i, line) in text.lines().enumerate() {
            let t = line.trim_start();
            if t.starts_with("async fn ")
                || t.starts_with("fn ")
                || t.starts_with("pub(crate) async fn ")
                || t.starts_with("pub(crate) fn ")
            {
                current_fn = t.to_string();
            }
            if t.starts_with("//") {
                continue;
            }
            if t.contains("transition_to_terminal(")
                && !t.contains("fn transition_to_terminal(")
                && !current_fn.contains("fn transition_to_terminal_settling_steers(")
            {
                offenders.push(format!("{}: {}", i + 1, t));
            }
        }
        assert!(
            offenders.is_empty(),
            "call transition_to_terminal_settling_steers instead:\n{}",
            offenders.join("\n")
        );
    }
}
