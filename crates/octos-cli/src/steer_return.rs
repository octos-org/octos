//! task-return-unconsumed-steer-inputs: build the `turn/steer_dropped`
//! notification that hands accepted-but-undrained `turn/steer` inputs back to
//! the client at turn end. Kept outside the `api` feature so its shape is
//! verifiable with a plain `cargo test -p octos-cli`; the `api` side
//! (`settle_leftover_steers`, invoked from the single terminal gate
//! `transition_to_terminal_settling_steers`) drains the buffer, calls this
//! and sends it — always BEFORE the terminal frame (`event.turn_steer_dropped.v1`).

use octos_core::SessionKey;
use octos_core::ui_protocol::{TurnId, TurnSteerDroppedEvent, UiNotification};

// No per-item allow needed: the only production reader is the allow-covered
// `leftover_steer_notification` below, and rustc seeds liveness from
// allow-covered items — so the consts stay quiet in no-`api` builds too.
/// `reason` when the turn was interrupted by the client.
pub(crate) const REASON_INTERRUPTED: &str = "interrupted";
/// `reason` when the turn ended on its own (EndTurn / error) with input pending.
pub(crate) const REASON_TURN_ENDED: &str = "turn_ended";

/// One `turn/steer_dropped` for the given leftovers, in buffer order. `None`
/// when there is nothing to return — an empty return frame is never emitted.
// The production drain-and-send site is `api`-gated
// (`settle_leftover_steers` in ui_protocol_transport.rs); the shape stays
// compiled feature-independent so plain `cargo test -p octos-cli` covers it.
#[cfg_attr(not(feature = "api"), allow(dead_code))]
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
