//! Channel trait and manager for gateway message routing.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use octos_core::{InboundMessage, OutboundMessage};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::bus::BusPublisher;
use crate::coalesce::{ChunkConfig, split_message};

/// A message channel (CLI, Telegram, Discord, etc.).
#[async_trait]
pub trait Channel: Send + Sync {
    /// Channel name used for routing (e.g. "cli", "telegram").
    fn name(&self) -> &str;

    /// Start listening for messages. Long-running — sends inbound messages via tx.
    async fn start(&self, inbound_tx: mpsc::Sender<InboundMessage>) -> Result<()>;

    /// Send an outbound message through this channel.
    async fn send(&self, msg: &OutboundMessage) -> Result<()>;

    /// Check if a sender is allowed to use this channel.
    fn is_allowed(&self, _sender_id: &str) -> bool {
        true
    }

    /// Maximum message length in characters for this channel.
    /// Messages exceeding this limit are automatically split at natural boundaries.
    fn max_message_length(&self) -> usize {
        4000
    }

    /// Stop the channel gracefully.
    async fn stop(&self) -> Result<()> {
        Ok(())
    }

    /// Send a typing/composing indicator. Platforms that don't support it return Ok(()).
    async fn send_typing(&self, _chat_id: &str) -> Result<()> {
        Ok(())
    }

    /// Send a typing/composing indicator as a specific sender identity when supported.
    /// Default: falls back to `send_typing()`.
    async fn send_typing_as(&self, chat_id: &str, _sender_user_id: Option<&str>) -> Result<()> {
        self.send_typing(chat_id).await
    }

    /// Send a "listening" / recording-voice indicator (for voice transcription).
    /// Falls back to typing indicator by default.
    async fn send_listening(&self, chat_id: &str) -> Result<()> {
        self.send_typing(chat_id).await
    }

    /// Whether this channel supports message editing (for progressive streaming).
    /// Channels that return `false` will not receive intermediate stream updates;
    /// only the final reply is sent.
    fn supports_edit(&self) -> bool {
        false
    }

    /// Send a message and return its platform message ID (for later editing/deletion).
    /// Default: delegates to `send()` and returns None.
    async fn send_with_id(&self, msg: &OutboundMessage) -> Result<Option<String>> {
        self.send(msg).await?;
        Ok(None)
    }

    /// Edit an existing message by platform message ID.
    async fn edit_message(
        &self,
        _chat_id: &str,
        _message_id: &str,
        _new_content: &str,
    ) -> Result<()> {
        Ok(())
    }

    /// Finalize a streamed message.
    ///
    /// Called once after the last streaming chunk. Channels that need special
    /// finalization (e.g. WeCom's `finish: true` stream frame) override this.
    /// Default: delegates to `edit_message()`.
    async fn finish_stream(
        &self,
        chat_id: &str,
        message_id: &str,
        final_content: &str,
    ) -> Result<()> {
        self.edit_message(chat_id, message_id, final_content).await
    }

    /// Delete a message by platform message ID.
    async fn delete_message(&self, _chat_id: &str, _message_id: &str) -> Result<()> {
        Ok(())
    }

    /// Edit a message with platform-specific metadata (e.g. inline keyboards).
    /// Default: ignores metadata and delegates to `edit_message()`.
    async fn edit_message_with_metadata(
        &self,
        chat_id: &str,
        message_id: &str,
        new_content: &str,
        _metadata: &serde_json::Value,
    ) -> Result<()> {
        self.edit_message(chat_id, message_id, new_content).await
    }

    /// Format outbound text for this channel's platform.
    ///
    /// Called by the outbound dispatcher before chunking. Channels that need
    /// platform-specific formatting (e.g. Markdown → HTML for Telegram) should
    /// override this. Default: returns content unchanged.
    fn format_outbound(&self, content: &str) -> String {
        content.to_string()
    }

    /// Add an emoji reaction to a message. Default: no-op.
    async fn react_to_message(
        &self,
        _chat_id: &str,
        _message_id: &str,
        _emoji: &str,
    ) -> Result<()> {
        Ok(())
    }

    /// Remove an emoji reaction from a message. Default: no-op.
    async fn remove_reaction(&self, _chat_id: &str, _message_id: &str, _emoji: &str) -> Result<()> {
        Ok(())
    }

    /// Send a rich embed message. Default: falls back to plain text.
    async fn send_embed(
        &self,
        chat_id: &str,
        title: &str,
        description: &str,
        fields: &[(String, String, bool)],
        _color: Option<u32>,
    ) -> Result<Option<String>> {
        let mut text = format!("**{title}**\n{description}");
        for (name, value, _inline) in fields {
            text.push_str(&format!("\n**{name}:** {value}"));
        }
        let msg = OutboundMessage {
            channel: self.name().to_string(),
            chat_id: chat_id.to_string(),
            content: text,
            reply_to: None,
            media: vec![],
            metadata: serde_json::json!({}),
        };
        self.send_with_id(&msg).await
    }

    /// Check channel health. Returns Ok(healthy) or Err with diagnosis.
    ///
    /// Used by the admin dashboard to show per-channel status. Default: unknown
    /// (returns Ok — assumed healthy if no probe implemented).
    async fn health_check(&self) -> Result<ChannelHealth> {
        Ok(ChannelHealth::Unknown)
    }
}

/// Health status reported by a channel's `health_check()`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "status", content = "detail")]
pub enum ChannelHealth {
    /// Channel is connected and operational.
    Healthy,
    /// Channel is partially working (e.g. rate-limited).
    Degraded(String),
    /// Channel is not reachable.
    Down(String),
    /// Channel does not implement health checks.
    Unknown,
}

/// Manages registered channels and dispatches outbound messages.
pub struct ChannelManager {
    channels: HashMap<String, Arc<dyn Channel>>,
}

impl Default for ChannelManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ChannelManager {
    pub fn new() -> Self {
        Self {
            channels: HashMap::new(),
        }
    }

    pub fn register(&mut self, channel: Arc<dyn Channel>) {
        self.channels.insert(channel.name().to_string(), channel);
    }

    /// Start all channels and the outbound dispatcher.
    /// Consumes the BusPublisher to own the outbound receiver.
    pub async fn start_all(&self, publisher: BusPublisher) -> Result<()> {
        // Decompose publisher: drop its in_tx so channels are the sole senders
        let inbound_tx = publisher.inbound_sender();
        let (publisher_in_tx, mut out_rx) = publisher.into_parts();
        drop(publisher_in_tx); // Drop publisher's own in_tx to prevent leak

        // Spawn each channel's listener
        for channel in self.channels.values() {
            let ch = Arc::clone(channel);
            let tx = inbound_tx.clone();
            tokio::spawn(async move {
                let name = ch.name().to_string();
                match ch.start(tx).await {
                    Ok(()) => {
                        warn!(channel = %name, "Channel listener exited cleanly (may need restart)")
                    }
                    Err(e) => error!(channel = %name, "Channel stopped with error: {e}"),
                }
            });
        }

        // Drop extra inbound sender so bus closes when all channels stop
        drop(inbound_tx);

        // Outbound dispatcher — routes messages to the correct channel
        let channels = self.channels.clone();
        let dispatcher_handle = tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if let Some(channel) = channels.get(&msg.channel) {
                    if !msg.media.is_empty() {
                        // File attachments: send directly without chunking
                        if let Err(e) = channel.send(&msg).await {
                            error!(
                                channel = msg.channel,
                                "Failed to send outbound file message: {e}",
                            );
                        }
                    } else if msg.content.is_empty() {
                        // Metadata-only message (e.g., completion signal) — deliver
                        // directly if metadata is present; skip empty messages otherwise.
                        if !msg.metadata.is_null() && msg.metadata != serde_json::json!({}) {
                            if let Err(e) = channel.send(&msg).await {
                                error!(
                                    channel = msg.channel,
                                    "Failed to send metadata message: {e}",
                                );
                            }
                        }
                    } else {
                        // Text message: format for platform, then chunk and send
                        let formatted = channel.format_outbound(&msg.content);
                        let config = ChunkConfig {
                            max_chars: channel.max_message_length(),
                        };
                        let chunks = split_message(&formatted, &config);
                        let total = chunks.len();
                        for (i, chunk) in chunks.into_iter().enumerate() {
                            let mut chunk_msg = msg.clone();
                            chunk_msg.content = chunk;
                            if let Err(e) = channel.send(&chunk_msg).await {
                                error!(
                                    channel = msg.channel,
                                    chunk = i + 1,
                                    total = total,
                                    "Failed to send outbound chunk: {e}",
                                );
                                break;
                            }
                        }
                    }
                } else {
                    error!(
                        channel = msg.channel,
                        "No channel registered for outbound message"
                    );
                }
            }
            info!("Outbound dispatcher stopped");
        });

        // Monitor dispatcher for panics
        tokio::spawn(async move {
            if let Err(e) = dispatcher_handle.await {
                error!("CRITICAL: outbound dispatcher panicked: {e}");
            }
        });

        Ok(())
    }

    /// Get a channel by name, for direct access (typing indicators, message editing).
    pub fn get_channel(&self, name: &str) -> Option<Arc<dyn Channel>> {
        self.channels.get(name).cloned()
    }

    pub async fn stop_all(&self) -> Result<()> {
        let mut errors = Vec::new();
        for channel in self.channels.values() {
            if let Err(e) = channel.stop().await {
                errors.push(format!("{}: {e}", channel.name()));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(eyre::eyre!(
                "failed to stop channels: {}",
                errors.join(", ")
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Mutex;

    struct MockChannel {
        channel_name: String,
        sent: Arc<Mutex<Vec<String>>>,
        start_count: Arc<AtomicUsize>,
    }

    impl MockChannel {
        fn new(name: &str) -> Self {
            Self {
                channel_name: name.to_string(),
                sent: Arc::new(Mutex::new(vec![])),
                start_count: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    #[async_trait]
    impl Channel for MockChannel {
        fn name(&self) -> &str {
            &self.channel_name
        }

        async fn start(&self, _inbound_tx: mpsc::Sender<InboundMessage>) -> Result<()> {
            self.start_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn send(&self, msg: &OutboundMessage) -> Result<()> {
            self.sent.lock().await.push(msg.content.clone());
            Ok(())
        }
    }

    #[test]
    fn test_register_channels() {
        let mut mgr = ChannelManager::new();
        mgr.register(Arc::new(MockChannel::new("ch1")));
        mgr.register(Arc::new(MockChannel::new("ch2")));
        assert_eq!(mgr.channels.len(), 2);
    }

    #[tokio::test]
    async fn test_dispatch_outbound() {
        let mock = Arc::new(MockChannel::new("test"));
        let sent = Arc::clone(&mock.sent);

        let mut mgr = ChannelManager::new();
        mgr.register(mock);

        let (agent, publisher) = crate::bus::create_bus();
        mgr.start_all(publisher).await.unwrap();

        agent
            .send_outbound(OutboundMessage {
                channel: "test".into(),
                chat_id: "c1".into(),
                content: "hello from agent".into(),
                reply_to: None,
                media: vec![],
                metadata: serde_json::json!({}),
            })
            .await
            .unwrap();

        // Drop agent handle to close the outbound channel
        drop(agent);

        // Give dispatcher time to process
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let messages = sent.lock().await;
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0], "hello from agent");
    }

    #[test]
    fn test_default_is_allowed() {
        let ch = MockChannel::new("test");
        assert!(ch.is_allowed("anyone"));
        assert!(ch.is_allowed(""));
    }

    #[test]
    fn test_default_max_message_length() {
        let ch = MockChannel::new("test");
        assert_eq!(ch.max_message_length(), 4000);
    }

    #[tokio::test]
    async fn test_default_stop() {
        let ch = MockChannel::new("test");
        assert!(ch.stop().await.is_ok());
    }

    #[tokio::test]
    async fn test_default_send_typing() {
        let ch = MockChannel::new("test");
        assert!(ch.send_typing("chat1").await.is_ok());
    }

    #[tokio::test]
    async fn test_default_edit_message() {
        let ch = MockChannel::new("test");
        assert!(
            ch.edit_message("chat1", "msg1", "new content")
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn test_default_delete_message() {
        let ch = MockChannel::new("test");
        assert!(ch.delete_message("chat1", "msg1").await.is_ok());
    }

    #[tokio::test]
    async fn test_send_with_id_returns_none() {
        let ch = MockChannel::new("test");
        let msg = OutboundMessage {
            channel: "test".into(),
            chat_id: "c1".into(),
            content: "hello".into(),
            reply_to: None,
            media: vec![],
            metadata: serde_json::json!({}),
        };
        let result = ch.send_with_id(&msg).await.unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_get_channel_found() {
        let mut mgr = ChannelManager::new();
        mgr.register(Arc::new(MockChannel::new("ch1")));
        assert!(mgr.get_channel("ch1").is_some());
    }

    #[test]
    fn test_get_channel_not_found() {
        let mgr = ChannelManager::new();
        assert!(mgr.get_channel("nonexistent").is_none());
    }

    #[test]
    fn test_channel_manager_default() {
        let mgr = ChannelManager::default();
        assert!(mgr.channels.is_empty());
    }

    #[tokio::test]
    async fn test_inbound_from_channel() {
        let (mut agent, publisher) = crate::bus::create_bus();
        let tx = publisher.inbound_sender();

        tx.send(InboundMessage {
            channel: "mock".into(),
            sender_id: "user1".into(),
            chat_id: "chat1".into(),
            content: "user says hi".into(),
            timestamp: Utc::now(),
            media: vec![],
            metadata: serde_json::json!({}),
            message_id: None,
        })
        .await
        .unwrap();

        let msg = agent.recv_inbound().await.unwrap();
        assert_eq!(msg.content, "user says hi");
    }
}
