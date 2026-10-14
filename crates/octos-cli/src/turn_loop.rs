//! task-interrupt-breaks-progress-wait: the one place the standalone-turn
//! event loop decides what to do next. Extracted so the interrupt-vs-progress
//! race is unit-testable: an interrupt must be reported the moment it lands,
//! never "after the next progress event" — a long tool with no output
//! (`bash sleep …`) used to hold the terminal back until the ~8 s status_word
//! heartbeat, so the client's 5 s `turn/interrupt` ack timed out.

use tokio::sync::mpsc;

/// What the turn loop should do next.
// No per-item allow needed: the only constructor/matcher is the
// allow-covered `next_turn_loop_step` below, and rustc seeds liveness from
// allow-covered items — so the enum stays quiet in no-`api` builds too.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TurnLoopStep {
    /// The interrupt signal arrived (reported once — the caller flips its
    /// `interrupt_observed` flag and breaks out of the loop immediately).
    Interrupted,
    /// A progress event payload from the agent task.
    Progress(String),
    /// The agent task's body has ended, so its completion-sender clone was
    /// dropped. Unlike `Closed` — which needs every progress sender dropped
    /// and stays unreachable while the rotator, the delivery worker, and the
    /// task supervisor keep theirs alive — this fires the moment the agent
    /// finishes, even when its terminal event was shed by the bounded
    /// terminal send (#2807), so the loop can always reach a terminal.
    AgentFinished,
    /// The progress channel closed: the agent task is gone.
    Closed,
}

/// Race the interrupt signal against the next progress event, interrupt
/// first (`biased`). Once `interrupt_observed` is set the interrupt arm is
/// disabled, mirroring the loop's original guard, so a caller that keeps
/// draining after an interrupt only ever sees progress/closed.
// Called only from `run_standalone_turn` in the `api` transport (plus the
// tests below).
#[cfg_attr(not(feature = "api"), allow(dead_code))]
pub(crate) async fn next_turn_loop_step(
    interrupt_rx: &mut mpsc::Receiver<()>,
    progress_rx: &mut mpsc::Receiver<String>,
    agent_finished_rx: &mut mpsc::Receiver<()>,
    interrupt_observed: bool,
) -> TurnLoopStep {
    tokio::select! {
        biased;
        _ = interrupt_rx.recv(), if !interrupt_observed => TurnLoopStep::Interrupted,
        recv = progress_rx.recv() => match recv {
            Some(data) => TurnLoopStep::Progress(data),
            None => TurnLoopStep::Closed,
        },
        // Biased after progress: the agent drops its completion sender only
        // after its terminal send resolved (delivered or shed), so a queued
        // terminal event is always drained by the progress arm first — this
        // arm only ends a turn whose terminal event never made it.
        _ = agent_finished_rx.recv() => TurnLoopStep::AgentFinished,
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
        let (_finished_tx, mut finished_rx) = mpsc::channel::<()>(1);
        // A "long tool with no output": nothing ever lands on progress_rx.
        interrupt_tx.send(()).await.unwrap();
        let step = tokio::time::timeout(
            Duration::from_millis(100),
            next_turn_loop_step(&mut interrupt_rx, &mut progress_rx, &mut finished_rx, false),
        )
        .await
        .expect("interrupt must wake the loop without any progress event");
        assert_eq!(step, TurnLoopStep::Interrupted);
    }

    #[tokio::test]
    async fn progress_events_flow_when_no_interrupt() {
        let (_interrupt_tx, mut interrupt_rx) = mpsc::channel::<()>(1);
        let (progress_tx, mut progress_rx) = mpsc::channel::<String>(8);
        let (_finished_tx, mut finished_rx) = mpsc::channel::<()>(1);
        progress_tx
            .send("{\"type\":\"delta\"}".into())
            .await
            .unwrap();
        let step =
            next_turn_loop_step(&mut interrupt_rx, &mut progress_rx, &mut finished_rx, false).await;
        assert_eq!(step, TurnLoopStep::Progress("{\"type\":\"delta\"}".into()));
    }

    #[tokio::test]
    async fn interrupt_wins_over_ready_progress() {
        let (interrupt_tx, mut interrupt_rx) = mpsc::channel::<()>(1);
        let (progress_tx, mut progress_rx) = mpsc::channel::<String>(8);
        let (_finished_tx, mut finished_rx) = mpsc::channel::<()>(1);
        progress_tx
            .send("{\"type\":\"delta\"}".into())
            .await
            .unwrap();
        interrupt_tx.send(()).await.unwrap();
        let step =
            next_turn_loop_step(&mut interrupt_rx, &mut progress_rx, &mut finished_rx, false).await;
        assert_eq!(step, TurnLoopStep::Interrupted, "biased: interrupt first");
    }

    #[tokio::test]
    async fn observed_interrupt_is_not_reported_twice_and_closed_channel_ends_loop() {
        let (interrupt_tx, mut interrupt_rx) = mpsc::channel::<()>(1);
        let (progress_tx, mut progress_rx) = mpsc::channel::<String>(8);
        let (_finished_tx, mut finished_rx) = mpsc::channel::<()>(1);
        interrupt_tx.send(()).await.unwrap();
        drop(progress_tx);
        // Already observed: the interrupt arm is disabled; the closed progress
        // channel ends the loop instead.
        let step =
            next_turn_loop_step(&mut interrupt_rx, &mut progress_rx, &mut finished_rx, true).await;
        assert_eq!(step, TurnLoopStep::Closed);
    }

    /// #2807: the agent task's completion signal ends the loop even though
    /// the progress channel is still open — the rotator/worker/supervisor
    /// keep their sender clones alive, so `Closed` alone never fires.
    #[tokio::test]
    async fn agent_finished_wakes_the_loop_without_progress_events() {
        let (_interrupt_tx, mut interrupt_rx) = mpsc::channel::<()>(1);
        let (_progress_tx, mut progress_rx) = mpsc::channel::<String>(8);
        let (finished_tx, mut finished_rx) = mpsc::channel::<()>(1);
        // The agent body ended: its completion-sender clone was dropped.
        drop(finished_tx);
        let step = tokio::time::timeout(
            Duration::from_millis(100),
            next_turn_loop_step(&mut interrupt_rx, &mut progress_rx, &mut finished_rx, false),
        )
        .await
        .expect("agent completion must wake the loop with the progress channel still open");
        assert_eq!(step, TurnLoopStep::AgentFinished);
    }

    /// Biased after progress: a terminal event queued before the agent's
    /// completion sender dropped is drained by the progress arm first, so
    /// the loop never reports AgentFinished while a terminal event awaits.
    #[tokio::test]
    async fn queued_progress_drains_before_agent_finished() {
        let (_interrupt_tx, mut interrupt_rx) = mpsc::channel::<()>(1);
        let (progress_tx, mut progress_rx) = mpsc::channel::<String>(8);
        let (finished_tx, mut finished_rx) = mpsc::channel::<()>(1);
        progress_tx
            .send("{\"type\":\"done\"}".into())
            .await
            .unwrap();
        drop(finished_tx);
        let step =
            next_turn_loop_step(&mut interrupt_rx, &mut progress_rx, &mut finished_rx, false).await;
        assert_eq!(
            step,
            TurnLoopStep::Progress("{\"type\":\"done\"}".into()),
            "biased: queued progress (including the terminal event) first"
        );
        let step =
            next_turn_loop_step(&mut interrupt_rx, &mut progress_rx, &mut finished_rx, false).await;
        assert_eq!(step, TurnLoopStep::AgentFinished);
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

    /// Structural guard (#2807): the production loop must feed the agent
    /// completion channel into the step helper — without the
    /// `AgentFinished` arm, a turn whose terminal event was shed parks in
    /// `recv()` forever, because the rotator/worker/supervisor keep the
    /// progress channel open past the agent task.
    #[test]
    fn standalone_turn_loop_feeds_the_agent_completion_signal() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/api/ui_protocol_transport.rs");
        let text = std::fs::read_to_string(&path).expect("read ui_protocol_transport.rs");
        let start = text
            .find("async fn run_standalone_turn")
            .expect("run_standalone_turn exists");
        let body = &text[start..];
        let call = body
            .find("crate::turn_loop::next_turn_loop_step(")
            .expect("the loop calls next_turn_loop_step");
        let call_args = &body[call..call + 600];
        assert!(
            call_args.contains("&mut agent_finished_rx"),
            "the drain loop must select on the agent completion channel"
        );
        // The match on the step result sits immediately after the call;
        // window it so a mention anywhere later in the file cannot satisfy
        // this guard.
        let match_window = &body[call..call + 4000];
        assert!(
            match_window.contains("crate::turn_loop::TurnLoopStep::AgentFinished"),
            "the loop must terminate on AgentFinished, not only on Closed"
        );
    }

    /// Structural guard (#2807): the agent task's directly-sent terminal
    /// events must go through the bounded helper — a bare `send().await`
    /// into the shared channel parks the task (and the turn's final
    /// `agent_task.await`) indefinitely on a stalled consumer.
    #[test]
    fn agent_terminal_events_are_bounded_against_a_stalled_consumer() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/api/ui_protocol_transport.rs");
        let text = std::fs::read_to_string(&path).expect("read ui_protocol_transport.rs");
        let start = text
            .find("let agent_task = tokio::spawn")
            .expect("the agent task spawn exists");
        let end = text[start..]
            .find("let _abort_guard = AbortOnDrop")
            .expect("the spawn block ends at the abort guard");
        let body = &text[start..start + end];
        assert!(
            body.matches("deliver_terminal_progress_event(").count() == 3,
            "all three terminal sends (error/incomplete/done) must be bounded"
        );
        assert!(
            !body.contains("progress_tx_for_result.send("),
            "no bare unbounded terminal send may remain in the agent task"
        );
    }
}
