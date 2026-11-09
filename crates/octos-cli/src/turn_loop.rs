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
