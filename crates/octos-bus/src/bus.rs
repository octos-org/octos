//! Message bus with typed handles for inbound/outbound routing.

use octos_core::{InboundMessage, OutboundMessage};
use tokio::sync::mpsc;

const CHANNEL_SIZE: usize = 4096;

/// Handle given to the agent loop — receives inbound, sends outbound.
pub struct AgentHandle {
    in_rx: mpsc::Receiver<InboundMessage>,
    out_tx: mpsc::Sender<OutboundMessage>,
}

impl AgentHandle {
    pub async fn recv_inbound(&mut self) -> Option<InboundMessage> {
        self.in_rx.recv().await
    }

    /// Drain all currently buffered inbound messages without blocking.
    pub fn try_recv_all(&mut self) -> Vec<InboundMessage> {
        let mut msgs = Vec::new();
        while let Ok(msg) = self.in_rx.try_recv() {
            msgs.push(msg);
        }
        msgs
    }

    pub async fn send_outbound(
        &self,
        msg: OutboundMessage,
    ) -> Result<(), mpsc::error::SendError<OutboundMessage>> {
        self.out_tx.send(msg).await
    }

    /// Clone the outbound sender for use by tools (e.g. MessageTool).
    pub fn outbound_sender(&self) -> mpsc::Sender<OutboundMessage> {
        self.out_tx.clone()
    }
}

/// Handle given to channels — sends inbound, receives outbound for dispatch.
pub struct BusPublisher {
    in_tx: mpsc::Sender<InboundMessage>,
    out_rx: mpsc::Receiver<OutboundMessage>,
}

impl BusPublisher {
    pub fn inbound_sender(&self) -> mpsc::Sender<InboundMessage> {
        self.in_tx.clone()
    }

    pub async fn recv_outbound(&mut self) -> Option<OutboundMessage> {
        self.out_rx.recv().await
    }

    /// Decompose into parts. Allows dropping the publisher's own `in_tx`
    /// so channels are the sole senders, enabling clean shutdown when all
    /// channels stop.
    pub fn into_parts(
        self,
    ) -> (
        mpsc::Sender<InboundMessage>,
        mpsc::Receiver<OutboundMessage>,
    ) {
        (self.in_tx, self.out_rx)
    }
}

/// Creates a linked pair of bus handles.
pub fn create_bus() -> (AgentHandle, BusPublisher) {
    let (in_tx, in_rx) = mpsc::channel(CHANNEL_SIZE);
    let (out_tx, out_rx) = mpsc::channel(CHANNEL_SIZE);
    (
        AgentHandle { in_rx, out_tx },
        BusPublisher { in_tx, out_rx },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn make_inbound(content: &str) -> InboundMessage {
        InboundMessage {
            channel: "test".into(),
            sender_id: "user1".into(),
            chat_id: "chat1".into(),
            content: content.into(),
            timestamp: Utc::now(),
            media: vec![],
            metadata: serde_json::json!({}),
            message_id: None,
        }
    }

    fn make_outbound(content: &str) -> OutboundMessage {
        OutboundMessage {
            channel: "test".into(),
            chat_id: "chat1".into(),
            content: content.into(),
            reply_to: None,
            media: vec![],
            metadata: serde_json::json!({}),
        }
    }

    #[tokio::test]
    async fn test_send_recv_inbound() {
        let (mut agent, publisher) = create_bus();
        let tx = publisher.inbound_sender();
        tx.send(make_inbound("hello")).await.unwrap();
        let msg = agent.recv_inbound().await.unwrap();
        assert_eq!(msg.content, "hello");
    }

    #[tokio::test]
    async fn test_send_recv_outbound() {
        let (agent, mut publisher) = create_bus();
        agent
            .send_outbound(make_outbound("response"))
            .await
            .unwrap();
        let msg = publisher.recv_outbound().await.unwrap();
        assert_eq!(msg.content, "response");
    }

    #[tokio::test]
    async fn test_dropped_sender_closes_receiver() {
        let (mut agent, publisher) = create_bus();
        drop(publisher);
        assert!(agent.recv_inbound().await.is_none());
    }

    #[tokio::test]
    async fn test_multiple_inbound_messages() {
        let (mut agent, publisher) = create_bus();
        let tx = publisher.inbound_sender();
        tx.send(make_inbound("one")).await.unwrap();
        tx.send(make_inbound("two")).await.unwrap();
        tx.send(make_inbound("three")).await.unwrap();
        assert_eq!(agent.recv_inbound().await.unwrap().content, "one");
        assert_eq!(agent.recv_inbound().await.unwrap().content, "two");
        assert_eq!(agent.recv_inbound().await.unwrap().content, "three");
    }

    #[tokio::test]
    async fn test_try_recv_all_empty() {
        let (mut agent, _publisher) = create_bus();
        let msgs = agent.try_recv_all();
        assert!(msgs.is_empty());
    }

    #[tokio::test]
    async fn test_try_recv_all_drains_buffered() {
        let (mut agent, publisher) = create_bus();
        let tx = publisher.inbound_sender();
        tx.send(make_inbound("a")).await.unwrap();
        tx.send(make_inbound("b")).await.unwrap();
        tx.send(make_inbound("c")).await.unwrap();
        let msgs = agent.try_recv_all();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].content, "a");
        assert_eq!(msgs[1].content, "b");
        assert_eq!(msgs[2].content, "c");
        // Subsequent call returns empty
        assert!(agent.try_recv_all().is_empty());
    }

    #[tokio::test]
    async fn test_clone_inbound_sender() {
        let (mut agent, publisher) = create_bus();
        let tx1 = publisher.inbound_sender();
        let tx2 = publisher.inbound_sender();
        tx1.send(make_inbound("from-tx1")).await.unwrap();
        tx2.send(make_inbound("from-tx2")).await.unwrap();
        let m1 = agent.recv_inbound().await.unwrap();
        let m2 = agent.recv_inbound().await.unwrap();
        assert_eq!(m1.content, "from-tx1");
        assert_eq!(m2.content, "from-tx2");
    }
}
