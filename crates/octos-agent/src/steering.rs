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
}

impl SteerBuffer {
    /// Append one steer input (FIFO position preserved).
    pub fn push(&self, text: String) {
        self.items
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(text);
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
/// BEFORE the next LLM call. Hosts use it to persist the injected user
/// message and emit their standard persisted-user-message event (codex
/// `record_user_prompt_and_emit_turn_item` parity). When a callback is
/// registered the host owns persistence of steer rows; the loop then keeps
/// them OUT of the turn output log so end-of-turn persistence cannot write
/// them a second time.
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
    /// Inject a user-role follow-up message into the conversation.
    FollowUp(Message),
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

#[cfg(test)]
mod tests {
    use super::*;
    use octos_core::MessageRole;

    #[tokio::test]
    async fn should_send_and_receive_follow_up() {
        let (tx, mut rx) = channel(DEFAULT_BUFFER);
        let msg = Message {
            role: MessageRole::User,
            content: "stop and focus on tests".into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        };
        tx.send(SteeringMessage::FollowUp(msg)).await.unwrap();
        let received = rx.recv().await.unwrap();
        assert!(
            matches!(received, SteeringMessage::FollowUp(m) if m.content == "stop and focus on tests")
        );
    }

    #[tokio::test]
    async fn should_drain_multiple_pending() {
        let (tx, mut rx) = channel(DEFAULT_BUFFER);
        tx.send(SteeringMessage::SystemReminder("hint 1".into()))
            .await
            .unwrap();
        tx.send(SteeringMessage::SystemReminder("hint 2".into()))
            .await
            .unwrap();
        tx.send(SteeringMessage::Cancel).await.unwrap();
        let pending = drain_pending(&mut rx);
        assert_eq!(pending.len(), 3);
        assert!(matches!(&pending[2], SteeringMessage::Cancel));
    }

    #[tokio::test]
    async fn should_return_empty_when_no_pending() {
        let (_tx, mut rx) = channel(DEFAULT_BUFFER);
        let pending = drain_pending(&mut rx);
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn should_handle_request_pause() {
        let (tx, mut rx) = channel(DEFAULT_BUFFER);
        tx.send(SteeringMessage::RequestPause).await.unwrap();
        let msg = rx.recv().await.unwrap();
        assert!(matches!(msg, SteeringMessage::RequestPause));
    }

    #[test]
    fn should_drain_steer_buffer_fifo_when_pushed_in_order() {
        let buffer = SteerBuffer::default();
        assert!(buffer.is_empty());
        buffer.push("first".to_string());
        buffer.push("second".to_string());
        assert!(!buffer.is_empty());
        assert_eq!(buffer.drain(), vec!["first", "second"]);
        assert!(buffer.is_empty());
        assert!(buffer.drain().is_empty());
    }

    #[test]
    fn should_keep_later_pushes_when_drained_between() {
        let buffer = SteerBuffer::default();
        buffer.push("a".to_string());
        assert_eq!(buffer.drain(), vec!["a"]);
        buffer.push("b".to_string());
        assert_eq!(buffer.drain(), vec!["b"]);
    }
}
