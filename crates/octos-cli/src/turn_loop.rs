//! task-interrupt-breaks-progress-wait: the one place the standalone-turn
//! event loop decides what to do next. Extracted so the interrupt-vs-progress
//! race is unit-testable: an interrupt must be reported the moment it lands,
//! never "after the next progress event" — a long tool with no output
//! (`bash sleep …`) used to hold the terminal back until the ~8 s status_word
//! heartbeat, so the client's 5 s `turn/interrupt` ack timed out.

use tokio::sync::mpsc;

/// What the turn loop should do next.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TurnLoopStep {
    /// The interrupt signal arrived (reported once — the caller flips its
    /// `interrupt_observed` flag and breaks out of the loop immediately).
    Interrupted,
    /// A progress event payload from the agent task.
    Progress(String),
    /// The progress channel closed: the agent task is gone.
    Closed,
}

/// Race the interrupt signal against the next progress event, interrupt
/// first (`biased`). Once `interrupt_observed` is set the interrupt arm is
/// disabled, mirroring the loop's original guard, so a caller that keeps
/// draining after an interrupt only ever sees progress/closed.
pub(crate) async fn next_turn_loop_step(
    interrupt_rx: &mut mpsc::Receiver<()>,
    progress_rx: &mut mpsc::Receiver<String>,
    interrupt_observed: bool,
) -> TurnLoopStep {
    tokio::select! {
        biased;
        _ = interrupt_rx.recv(), if !interrupt_observed => TurnLoopStep::Interrupted,
        recv = progress_rx.recv() => match recv {
            Some(data) => TurnLoopStep::Progress(data),
            None => TurnLoopStep::Closed,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn interrupt_returns_immediately_without_progress_events() {
        let (interrupt_tx, mut interrupt_rx) = mpsc::channel::<()>(1);
        let (_progress_tx, mut progress_rx) = mpsc::channel::<String>(8);
        // A "long tool with no output": nothing ever lands on progress_rx.
        interrupt_tx.send(()).await.unwrap();
        let step = tokio::time::timeout(
            Duration::from_millis(100),
            next_turn_loop_step(&mut interrupt_rx, &mut progress_rx, false),
        )
        .await
        .expect("interrupt must wake the loop without any progress event");
        assert_eq!(step, TurnLoopStep::Interrupted);
    }

    #[tokio::test]
    async fn progress_events_flow_when_no_interrupt() {
        let (_interrupt_tx, mut interrupt_rx) = mpsc::channel::<()>(1);
        let (progress_tx, mut progress_rx) = mpsc::channel::<String>(8);
        progress_tx
            .send("{\"type\":\"delta\"}".into())
            .await
            .unwrap();
        let step = next_turn_loop_step(&mut interrupt_rx, &mut progress_rx, false).await;
        assert_eq!(step, TurnLoopStep::Progress("{\"type\":\"delta\"}".into()));
    }

    #[tokio::test]
    async fn interrupt_wins_over_ready_progress() {
        let (interrupt_tx, mut interrupt_rx) = mpsc::channel::<()>(1);
        let (progress_tx, mut progress_rx) = mpsc::channel::<String>(8);
        progress_tx
            .send("{\"type\":\"delta\"}".into())
            .await
            .unwrap();
        interrupt_tx.send(()).await.unwrap();
        let step = next_turn_loop_step(&mut interrupt_rx, &mut progress_rx, false).await;
        assert_eq!(step, TurnLoopStep::Interrupted, "biased: interrupt first");
    }

    #[tokio::test]
    async fn observed_interrupt_is_not_reported_twice_and_closed_channel_ends_loop() {
        let (interrupt_tx, mut interrupt_rx) = mpsc::channel::<()>(1);
        let (progress_tx, mut progress_rx) = mpsc::channel::<String>(8);
        interrupt_tx.send(()).await.unwrap();
        drop(progress_tx);
        // Already observed: the interrupt arm is disabled; the closed progress
        // channel ends the loop instead.
        let step = next_turn_loop_step(&mut interrupt_rx, &mut progress_rx, true).await;
        assert_eq!(step, TurnLoopStep::Closed);
    }

    /// Structural guard: the production loop takes its next step from this
    /// helper and breaks on `Interrupted` — the `continue`-then-wait shape
    /// that caused the latency must not come back.
    #[test]
    fn standalone_turn_loop_breaks_on_interrupt_step() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/api/ui_protocol_transport.rs");
        let text = std::fs::read_to_string(&path).expect("read ui_protocol_transport.rs");
        let start = text
            .find("async fn run_standalone_turn")
            .expect("run_standalone_turn exists");
        let body = &text[start..];
        assert!(
            body.contains("crate::turn_loop::next_turn_loop_step("),
            "run_standalone_turn must take its next step from turn_loop::next_turn_loop_step"
        );
        let mut lines = body.lines().peekable();
        while let Some(line) = lines.next() {
            if line.trim() == "interrupt_observed = true;" {
                if let Some(next) = lines.peek() {
                    assert_ne!(
                        next.trim(),
                        "continue;",
                        "interrupt observation must break out of the loop, not continue into another progress wait"
                    );
                }
            }
        }
    }
}
