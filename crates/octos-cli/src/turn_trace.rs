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
pub(crate) fn turn_span(session_id: &SessionKey, turn_id: &TurnId) -> Span {
    info_span!("turn", session = %session_id.0, turn = %turn_id.0)
}

/// `turn/interrupt` reached the server for this session/turn.
pub(crate) fn log_interrupt_received(session_id: &SessionKey, turn_id: &TurnId) {
    info!(session = %session_id.0, turn = %turn_id.0, "turn/interrupt received");
}

/// How the interrupt was decided: `captured`, `already_interrupting`,
/// `already_terminal:<reason>`, `mismatch` or `unknown`.
pub(crate) fn log_interrupt_outcome(session_id: &SessionKey, turn_id: &TurnId, outcome: &str) {
    info!(session = %session_id.0, turn = %turn_id.0, %outcome, "turn/interrupt decided");
}

/// The captured interrupt's ack result: `interrupted` or `ack_timed_out`.
pub(crate) fn log_interrupt_ack(session_id: &SessionKey, turn_id: &TurnId, ack: &str) {
    info!(session = %session_id.0, turn = %turn_id.0, %ack, "turn/interrupt acknowledged");
}

/// `turn/steer` accepted into the turn's pending-input buffer.
/// `interrupting = true` means the turn was already winding down when the
/// input was accepted — it will most likely be returned as
/// `turn/steer_dropped` rather than drained.
pub(crate) fn log_steer_accepted(session_id: &SessionKey, turn_id: &TurnId, interrupting: bool) {
    info!(
        session = %session_id.0,
        turn = %turn_id.0,
        interrupting,
        "turn/steer accepted into the active turn's pending-input buffer"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

    impl Write for CapturedLogs {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn capture(run: impl FnOnce()) -> String {
        let captured = CapturedLogs::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_target(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, run);
        String::from_utf8(captured.0.lock().unwrap().clone()).unwrap()
    }

    fn ids() -> (SessionKey, TurnId) {
        (SessionKey("local:tui#coding".into()), TurnId::new())
    }

    #[test]
    fn interrupt_lifecycle_logs_carry_session_and_turn() {
        let (session, turn) = ids();
        let logs = capture(|| {
            log_interrupt_received(&session, &turn);
            log_interrupt_outcome(&session, &turn, "captured");
            log_interrupt_ack(&session, &turn, "interrupted");
        });
        let lines: Vec<&str> = logs.lines().collect();
        assert_eq!(lines.len(), 3, "{logs}");
        for line in &lines {
            assert!(line.contains("session=local:tui#coding"), "{line}");
            assert!(line.contains(&format!("turn={}", turn.0)), "{line}");
            assert!(
                line.starts_with(" INFO") || line.starts_with("INFO"),
                "{line}"
            );
        }
        assert!(lines[0].contains("turn/interrupt received"));
        assert!(lines[1].contains("outcome=captured"), "{:?}", lines[1]);
        assert!(
            !lines[1].contains("outcome=\"captured\""),
            "Display, not Debug: {:?}",
            lines[1]
        );
        assert!(lines[2].contains("ack=interrupted"));
    }

    #[test]
    fn agent_logs_inside_turn_span_carry_session_and_turn() {
        let (session, turn) = ids();
        let logs = capture(|| {
            let span = turn_span(&session, &turn);
            let _guard = span.enter();
            tracing::info!("calling LLM iteration=6");
        });
        assert!(logs.contains("turn{"), "span name is printed: {logs}");
        assert!(logs.contains("session=local:tui#coding"), "{logs}");
        assert!(logs.contains(&format!("turn={}", turn.0)), "{logs}");
        assert!(logs.contains("calling LLM iteration=6"));
    }

    #[test]
    fn steer_accepted_log_marks_interrupting_turns() {
        let (session, turn) = ids();
        let logs = capture(|| {
            log_steer_accepted(&session, &turn, false);
            log_steer_accepted(&session, &turn, true);
        });
        let lines: Vec<&str> = logs.lines().collect();
        assert_eq!(lines.len(), 2, "{logs}");
        assert!(
            lines[0].contains("interrupting=false")
                && lines[0].contains("session=")
                && lines[0].contains("turn=")
        );
        assert!(
            lines[1].contains("interrupting=true")
                && lines[1].contains("session=")
                && lines[1].contains("turn=")
        );
    }

    #[test]
    fn correlation_logs_never_contain_user_text() {
        let (session, turn) = ids();
        let steer_text = "SECRET-MARKER-please-rm-rf-nothing";
        let logs = capture(|| {
            // The text lives with the caller; none of the log fns take it.
            let _pending = [steer_text.to_string()];
            log_interrupt_received(&session, &turn);
            log_interrupt_outcome(&session, &turn, "captured");
            log_interrupt_ack(&session, &turn, "interrupted");
            log_steer_accepted(&session, &turn, true);
        });
        assert!(!logs.contains(steer_text), "{logs}");
        assert!(!logs.contains("SECRET-MARKER"));
    }

    #[test]
    fn interrupt_outcome_logs_cover_rejections_at_info() {
        let (session, turn) = ids();
        let logs = capture(|| {
            log_interrupt_outcome(&session, &turn, "unknown");
            log_interrupt_outcome(&session, &turn, "mismatch");
            log_interrupt_outcome(&session, &turn, "already_terminal:interrupted");
        });
        let lines: Vec<&str> = logs.lines().collect();
        assert_eq!(lines.len(), 3, "{logs}");
        assert!(lines[0].contains("outcome=unknown") && lines[0].contains("INFO"));
        assert!(lines[1].contains("outcome=mismatch") && lines[1].contains("INFO"));
        assert!(
            lines[2].contains("outcome=already_terminal:interrupted") && lines[2].contains("INFO")
        );
        assert!(!logs.contains("WARN") && !logs.contains("ERROR"));
    }
}
