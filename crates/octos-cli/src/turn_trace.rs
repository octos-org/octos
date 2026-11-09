//! task-turn-interrupt-steer-correlation-logs: session/turn-correlated
//! lifecycle logging for `turn/interrupt` and `turn/steer`.
//!
//! The 2026-08-17 incident had to be reconstructed from the ledger because
//! the server log carried no INFO record of the interrupt and the agent-side
//! lines (`calling LLM`, `executing tool batch`, `draining mid-turn steer
//! input`) named neither session nor turn. Everything here logs ids, counts
//! and states only — never user text.

use octos_core::SessionKey;
use octos_core::ui_protocol::TurnId;
use tracing::{Span, info, info_span};

/// The span the spawned agent future runs under, so every log line it emits
/// (LLM calls, tool batches, steer drains, EndTurn rounds) carries
/// `session` and `turn` without each call site naming them.
// Every call site lives in the `api` transport (ui_protocol_transport.rs);
// the fns stay compiled feature-independent so the no-user-text shape stays
// unit-testable.
#[cfg_attr(not(feature = "api"), allow(dead_code))]
pub(crate) fn turn_span(session_id: &SessionKey, turn_id: &TurnId) -> Span {
    info_span!("turn", session = %session_id.0, turn = %turn_id.0)
}

/// `turn/interrupt` reached the server for this session/turn.
#[cfg_attr(not(feature = "api"), allow(dead_code))]
pub(crate) fn log_interrupt_received(session_id: &SessionKey, turn_id: &TurnId) {
    info!(session = %session_id.0, turn = %turn_id.0, "turn/interrupt received");
}

/// How the interrupt was decided: `captured`, `already_interrupting`,
/// `already_terminal:<reason>`, `mismatch` or `unknown`.
#[cfg_attr(not(feature = "api"), allow(dead_code))]
pub(crate) fn log_interrupt_outcome(session_id: &SessionKey, turn_id: &TurnId, outcome: &str) {
    info!(session = %session_id.0, turn = %turn_id.0, %outcome, "turn/interrupt decided");
}

/// The captured interrupt's ack result: `interrupted` or `ack_timed_out`.
#[cfg_attr(not(feature = "api"), allow(dead_code))]
pub(crate) fn log_interrupt_ack(session_id: &SessionKey, turn_id: &TurnId, ack: &str) {
    info!(session = %session_id.0, turn = %turn_id.0, %ack, "turn/interrupt acknowledged");
}

/// `turn/steer` accepted into the turn's pending-input buffer.
/// `interrupting = true` means the turn was already winding down when the
/// input was accepted — it will most likely be returned as
/// `turn/steer_dropped` rather than drained.
#[cfg_attr(not(feature = "api"), allow(dead_code))]
pub(crate) fn log_steer_accepted(session_id: &SessionKey, turn_id: &TurnId, interrupting: bool) {
    info!(
        session = %session_id.0,
        turn = %turn_id.0,
        interrupting,
        "turn/steer accepted into the active turn's pending-input buffer"
    );
}
