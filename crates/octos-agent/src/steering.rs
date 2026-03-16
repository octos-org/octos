//! Steering message queue for injecting messages mid-session.
//!
//! Allows external callers (UI, API, hooks) to inject follow-up messages
//! into a running agent loop without waiting for the current turn to finish.
//!
//! TODO: Wire `SteeringReceiver` into the agent loop (`agent.rs`) to drain
//! pending messages between iterations and handle Cancel/RequestPause.

use octos_core::Message;
use tokio::sync::mpsc;

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
}
