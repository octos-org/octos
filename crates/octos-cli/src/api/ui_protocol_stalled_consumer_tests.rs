//! #2807 / UPCR-2026-048 — turn termination must not park on a stalled
//! consumer: the budgeted terminal shed, the drain loop's completion-signal
//! arm, and the cooperative stdio lifecycle lane.
use super::*;

/// The agent task's terminal event sheds and counts into the drop counter
/// once the stalled consumer holds the progress channel past the send
/// budget — instead of parking the task (and with it the turn) forever.
#[tokio::test(start_paused = true)]
async fn terminal_progress_event_sheds_and_counts_past_budget_on_stalled_channel() {
    let (tx, mut rx) = mpsc::channel::<String>(1);
    // The stalled consumer: the only slot is full and never drained.
    tx.send("queued".into()).await.unwrap();
    let dropped = Arc::new(AtomicU64::new(0));
    let shed = {
        let tx = tx.clone();
        let dropped = Arc::clone(&dropped);
        tokio::spawn(async move {
            deliver_terminal_progress_event(&tx, &dropped, "terminal".into()).await;
        })
    };
    tokio::time::timeout(std::time::Duration::from_secs(600), shed)
        .await
        .expect("terminal send must shed past the budget instead of parking")
        .expect("delivery task must not panic");
    assert_eq!(
        dropped.load(Ordering::Relaxed),
        1,
        "the shed terminal frame must be counted into the replay_lossy counter"
    );
    // The consumer resumes: only the queued row remains; the shed frame is
    // gone and the channel was not closed out from under other senders.
    assert_eq!(rx.recv().await.as_deref(), Some("queued"));
    assert_eq!(
        rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty),
        "helper must not close the channel"
    );
}

/// Healthy path: the terminal event delivers untouched and counts nothing.
#[tokio::test]
async fn terminal_progress_event_delivers_when_the_consumer_reads() {
    let (tx, mut rx) = mpsc::channel::<String>(1);
    let dropped = Arc::new(AtomicU64::new(0));
    deliver_terminal_progress_event(&tx, &dropped, "terminal".into()).await;
    assert_eq!(dropped.load(Ordering::Relaxed), 0);
    assert_eq!(rx.recv().await.as_deref(), Some("terminal"));
}

/// Assembly-level regression (#2807): the drain-loop shape built from the
/// REAL components — a saturated `PROGRESS_CHANNEL_CAPACITY` channel, the
/// real bounded terminal send, the real step helper — always terminates on
/// the completion signal within the send budget. Before the fix the agent
/// task parked on the unbounded terminal send and the loop parked in
/// `recv()` behind it forever. Whatever the interleaving (consumer drains
/// in time and the terminal event delivers, or the budget fires and the
/// frame sheds), the turn shape ends as `AgentFinished` and the terminal
/// frame is either delivered or counted — never lost in limbo.
#[tokio::test(start_paused = true)]
async fn stalled_consumer_turn_shape_terminates_within_budget() {
    let (progress_tx, mut progress_rx) = mpsc::channel::<String>(PROGRESS_CHANNEL_CAPACITY);
    let dropped = Arc::new(AtomicU64::new(0));
    let (_interrupt_tx, mut interrupt_rx) = mpsc::channel::<()>(1);
    let (finished_tx, mut finished_rx) = mpsc::channel::<()>(1);

    // The stalled consumer's residue: a shared channel full of progress
    // rows nobody has drained yet.
    for i in 0..PROGRESS_CHANNEL_CAPACITY {
        progress_tx.send(format!("row {i}")).await.unwrap();
    }

    // The agent task with the production body's exact exit shape: terminal
    // event through the bounded helper, then the completion sender drops.
    let agent_dropped = Arc::clone(&dropped);
    let agent = tokio::spawn(async move {
        deliver_terminal_progress_event(&progress_tx, &agent_dropped, "done".into()).await;
        drop(finished_tx);
    });

    // The drain loop, with the production shape: real step helper, break on
    // every terminal step.
    let loop_end = tokio::spawn(async move {
        loop {
            match crate::turn_loop::next_turn_loop_step(
                &mut interrupt_rx,
                &mut progress_rx,
                &mut finished_rx,
                false,
            )
            .await
            {
                crate::turn_loop::TurnLoopStep::Interrupted => return ("interrupted", false),
                crate::turn_loop::TurnLoopStep::Closed => return ("closed", false),
                crate::turn_loop::TurnLoopStep::AgentFinished => return ("agent-finished", true),
                crate::turn_loop::TurnLoopStep::Progress(data) => {
                    if data == "done" {
                        // The terminal event raced the budget and delivered.
                        return ("terminal-delivered", true);
                    }
                }
            }
        }
    });

    let (outcome, terminated) = tokio::time::timeout(std::time::Duration::from_secs(600), loop_end)
        .await
        .expect("the turn shape must terminate within the send budget, not park")
        .expect("loop task must not panic");
    let _ = agent.await;
    assert!(terminated, "the loop must end on a terminal step");
    match outcome {
        "terminal-delivered" => {
            assert_eq!(dropped.load(Ordering::Relaxed), 0);
        }
        "agent-finished" => {
            assert_eq!(
                dropped.load(Ordering::Relaxed),
                1,
                "a shed terminal frame must be counted, never lost in limbo"
            );
        }
        other => panic!("unexpected loop outcome: {other}"),
    }
}

/// The offloaded stdio lifecycle lane parks cooperatively until the host
/// reader drains the queue, then delivers — never pinning a worker on the
/// blocking send it replaces.
#[tokio::test]
async fn lifecycle_offloaded_stdio_lane_drains_a_full_queue_cooperatively() {
    let (writer_tx, writer_rx) = std::sync::mpsc::sync_channel::<WsMessage>(1);
    // Fill the single stdio slot first: the blocking send this lane replaces
    // would park here forever on a host that reads nothing.
    writer_tx
        .send(WsMessage::Text("queued".into()))
        .expect("fill the stdio slot");
    let ws = WsConnection::new_stdio(writer_tx);
    // The "host reader" resumes after a pause and drains both rows, then
    // sees the channel close when the test drops the connection.
    let reader = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(30));
        let mut rows = Vec::new();
        while let Ok(frame) = writer_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            if let WsMessage::Text(text) = frame {
                rows.push(text);
            }
        }
        rows
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        ws.send_lifecycle_offloaded(WsMessage::Text("terminal".into())),
    )
    .await
    .expect("offloaded send must complete once the host reader drains")
    .expect("offloaded lifecycle send must succeed on a live stdio lane");
    let rows = reader.join().expect("reader must not panic");
    assert_eq!(rows, ["queued", "terminal"]);
}

/// A disconnected stdio host latches the connection exactly like the sync
/// lane's error mapping does (#922.2): `LifecycleFailure` + `mark_failed`.
#[tokio::test]
async fn lifecycle_offloaded_stdio_disconnected_reports_closed() {
    let (writer_tx, writer_rx) = std::sync::mpsc::sync_channel::<WsMessage>(1);
    let ws = WsConnection::new_stdio(writer_tx);
    drop(writer_rx);
    let result = ws
        .send_lifecycle_offloaded(WsMessage::Text("terminal".into()))
        .await;
    assert!(matches!(result, Err(SendError::LifecycleFailure(_))));
    assert!(
        ws.is_failed(),
        "a dead stdio host must latch the connection"
    );
}
