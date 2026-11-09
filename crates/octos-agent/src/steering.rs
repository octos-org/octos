//! Steering message queue for injecting messages mid-session.
//!
//! Allows external callers (UI, API, hooks) to inject follow-up messages
//! into a running agent loop without waiting for the current turn to finish.
//!
//! TODO: Wire `SteeringReceiver` into the agent loop (`agent.rs`) to drain
//! pending messages between iterations and handle Cancel/RequestPause.

use octos_core::Message;
use tokio::sync::mpsc;

/// Per-turn pending-input buffer for mid-turn prompt injection ("steer").
///
/// Codex parity: mirrors `TurnState.pending_input` (`codex-rs
/// core/src/state/turn.rs` + `input_queue.rs`) — an append-order `Vec`
/// guarded by a mutex, drained FIFO via `split_off(0)` at the TOP of each
/// agent-loop iteration, before the next LLM call. Steer inputs land as
/// plain `role: user` messages with NO wrapper text.
///
/// The host (e.g. `octos serve`'s `turn/steer` RPC) holds one end via
/// [`SharedSteerBuffer`] and pushes under its own active-turn registry
/// lock; the agent loop holds the other via
/// [`crate::Agent::with_steer_buffer`]. Steering is NOT an interrupt:
/// the interrupt channel stays separate (codex `Op::Interrupt` vs
/// `Op::UserInput`).
#[derive(Debug, Default)]
pub struct SteerBuffer {
    items: std::sync::Mutex<Vec<String>>,
    changed: tokio::sync::Notify,
}

impl SteerBuffer {
    /// Append one steer input (FIFO position preserved).
    pub fn push(&self, text: String) {
        self.items
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(text);
        self.changed.notify_one();
    }

    /// Wake the current model request without polling. Register before checking
    /// the buffer so input arriving at the check/await boundary is not missed.
    pub async fn wait_for_input(&self) {
        loop {
            let changed = self.changed.notified();
            if !self.is_empty() {
                return;
            }
            changed.await;
        }
    }

    /// Drain every pending input in arrival order (codex `split_off(0)`).
    pub fn drain(&self) -> Vec<String> {
        let mut items = self.items.lock().unwrap_or_else(|error| error.into_inner());
        items.split_off(0)
    }

    /// Whether the buffer currently holds no pending input.
    pub fn is_empty(&self) -> bool {
        self.items
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_empty()
    }
}

/// Shared handle to a per-turn [`SteerBuffer`].
pub type SharedSteerBuffer = std::sync::Arc<SteerBuffer>;

/// Host callback observing each drained steer batch, called INLINE from the
/// agent loop right after the drained texts are appended to the prompt and
/// BEFORE the next LLM call. Hosts use it to emit the injected user
/// message echo (codex `record_user_prompt_and_emit_turn_item` parity).
/// The loop always retains the rows in its chronological turn output log;
/// hosts persist that log once, rather than persisting callback rows again.
pub type SteerDrainedCallback = std::sync::Arc<
    dyn Fn(Vec<String>) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

/// Sender half — held by callers who want to inject messages.
pub type SteeringSender = mpsc::Sender<SteeringMessage>;

/// Receiver half — consumed by the agent loop.
pub type SteeringReceiver = mpsc::Receiver<SteeringMessage>;

/// A message injected into the agent loop mid-session.
#[derive(Debug, Clone)]
pub enum SteeringMessage {
    /// Inject a user-role follow-up message into the conversation. Boxed so
    /// the rare follow-up does not size every steering signal like a `Message`.
    FollowUp(Box<Message>),
    /// Inject a system-role reminder (prepended to next LLM call).
    SystemReminder(String),
    /// Request the agent to pause and await input.
    RequestPause,
    /// Request the agent to cancel the current task.
    Cancel,
}

/// Create a steering channel with the given buffer size.
pub fn channel(buffer: usize) -> (SteeringSender, SteeringReceiver) {
    mpsc::channel(buffer)
}

/// Default buffer size for steering channels.
pub const DEFAULT_BUFFER: usize = 16;

/// Drain all pending steering messages from the receiver (non-blocking).
pub fn drain_pending(rx: &mut SteeringReceiver) -> Vec<SteeringMessage> {
    let mut messages = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        messages.push(msg);
    }
    messages
}
