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

/// Keeps channel admission ordered with deletion until actor dispatch completes.
/// Channels without admission tracking use the empty default guard.
pub struct InboundDispatchGuard {
    _lease: Box<dyn Send + Sync>,
}

impl Default for InboundDispatchGuard {
    fn default() -> Self {
        Self {
            _lease: Box::new(()),
        }
    }
}

impl InboundDispatchGuard {
    #[cfg(feature = "api")]
    pub(crate) fn new(lease: impl Send + Sync + 'static) -> Self {
        Self {
            _lease: Box::new(lease),
        }
    }
}

/// A message channel (CLI, Telegram, Discord, etc.).
#[async_trait]
pub trait Channel: Send + Sync {
    /// Channel name used for routing (e.g. "cli", "telegram").
    fn name(&self) -> &str;

    /// Start listening for messages. Long-running — sends inbound messages via tx.
    async fn start(&self, inbound_tx: mpsc::Sender<InboundMessage>) -> Result<()>;

    /// Send an outbound message through this channel.
    async fn send(&self, msg: &OutboundMessage) -> Result<()>;

    /// Bind ingress to the actual routed session before opening/enqueuing an
    /// actor. The caller holds the guard through dispatch; a rejected message
    /// must not create an actor or invoke its model.
    async fn admit_dispatch(
        &self,
        _message: &InboundMessage,
        _session: &octos_core::SessionKey,
    ) -> Result<InboundDispatchGuard> {
        Ok(InboundDispatchGuard::default())
    }

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

    /// Stop a typing/composing indicator. Platforms that don't support it return Ok(()).
    async fn stop_typing(&self, _chat_id: &str) -> Result<()> {
        Ok(())
    }

    /// Stop a typing/composing indicator as a specific sender identity when supported.
    /// Default: falls back to `stop_typing()`.
    async fn stop_typing_as(&self, chat_id: &str, _sender_user_id: Option<&str>) -> Result<()> {
        self.stop_typing(chat_id).await
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

    /// Edit an existing message with an explicit `thread_id` binding.
    ///
    /// PR F (M8.10 thread-binding chain `#649 → #740`): the originating
    /// turn's `thread_id` is captured at the stream forwarder's task spawn
    /// and threaded through every flush. Forwarding it explicitly here
    /// closes the wire-side leak where the legacy `edit_message` path
    /// recovered `thread_id` via `decode_sse_message_id` and then a
    /// per-chat sticky map — under rapid-fire concurrent turns the sticky
    /// has rotated, so late deltas of an earlier turn mis-routed to a
    /// later turn's bubble. Channels that don't track threads ignore this
    /// argument; the default impl forwards to legacy `edit_message`,
    /// preserving Telegram/Discord/etc. behavior bug-for-bug.
    async fn edit_message_bound(
        &self,
        chat_id: &str,
        message_id: &str,
        new_content: &str,
        _thread_id: Option<&str>,
    ) -> Result<()> {
        self.edit_message(chat_id, message_id, new_content).await
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

    /// Finalize a streamed message with an explicit `thread_id` binding.
    ///
    /// PR F sibling of [`edit_message_bound`]. Default: forwards to
    /// [`edit_message_bound`] so the bound argument propagates through
    /// the same `bound > decoded > sticky` resolution shape the API
    /// channel implements.
    async fn finish_stream_bound(
        &self,
        chat_id: &str,
        message_id: &str,
        final_content: &str,
        thread_id: Option<&str>,
    ) -> Result<()> {
        self.edit_message_bound(chat_id, message_id, final_content, thread_id)
            .await
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

    /// Send a raw SSE JSON event to the client (API channel only).
    ///
    /// Used by the stream forwarder to inject discrete progress events
    /// (thinking, tool_start, tool_progress, cost_update) into the SSE
    /// stream alongside the text-based streaming updates. Non-SSE channels
    /// (Telegram, Matrix, etc.) ignore this.
    async fn send_raw_sse(&self, _chat_id: &str, _json: &str) -> Result<()> {
        Ok(())
    }

    /// Send a raw SSE JSON event with an explicit `thread_id` binding.
    ///
    /// PR F (M8.10): the API channel implements this to override any
    /// `thread_id` on the JSON payload (or absence thereof) with the
    /// caller-supplied `bound` value before broadcasting. This closes the
    /// LEAK 2 wire path — the legacy `send_raw_sse` falls back to a
    /// per-chat sticky map that rotates under rapid-fire concurrent turns.
    /// Default impl forwards to [`send_raw_sse`] so non-SSE channels
    /// remain a strict no-op.
    async fn send_raw_sse_bound(
        &self,
        chat_id: &str,
        json: &str,
        _thread_id: Option<&str>,
    ) -> Result<()> {
        self.send_raw_sse(chat_id, json).await
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

/// Per-message deadline for one outbound `Channel::send`. A channel whose send
/// parks (half-open connection, stuck lock) used to block the serial
/// dispatcher forever, stalling every channel's outbound traffic (R-14). On
/// expiry the dispatcher logs and moves to the next message; the aborted send
/// is dropped, so a misbehaving channel cannot hold the queue hostage.
const CHANNEL_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

/// Manages registered channels and dispatches outbound messages.
pub struct ChannelManager {
    channels: HashMap<String, Arc<dyn Channel>>,
}

/// Send one outbound message with a hard deadline so a stuck channel cannot
/// wedge the serial dispatcher (R-14).
async fn send_with_deadline(channel: &dyn Channel, msg: &OutboundMessage) -> Result<()> {
    match tokio::time::timeout(CHANNEL_SEND_TIMEOUT, channel.send(msg)).await {
        Ok(result) => result,
        Err(_) => Err(eyre::eyre!(
            "send to channel '{}' timed out after {}s",
            channel.name(),
            CHANNEL_SEND_TIMEOUT.as_secs()
        )),
    }
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
    ///
    /// Returns `Err` if the dispatcher cannot start (e.g. the outbound bus is
    /// already closed). An `Err` is a fatal startup condition: channel
    /// listeners already spawned keep running until the runtime shuts down.
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

        // Outbound dispatcher — routes messages to the correct channel.
        // Reports startup readiness (or a dead outbound bus) via oneshot so
        // start_all fails instead of silently running without routing.
        let channels = self.channels.clone();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let dispatcher_handle = tokio::spawn(async move {
            if out_rx.is_closed() {
                let _ = ready_tx.send(Err("outbound bus closed before dispatcher start"));
                return;
            }
            let _ = ready_tx.send(Ok(()));
            while let Some(msg) = out_rx.recv().await {
                if let Some(channel) = channels.get(&msg.channel) {
                    if !msg.media.is_empty() {
                        // File attachments: send directly without chunking
                        if let Err(e) = send_with_deadline(&**channel, &msg).await {
                            error!(
                                channel = msg.channel,
                                "Failed to send outbound file message: {e}",
                            );
                        }
                    } else if msg.content.is_empty() {
                        // Metadata-only message (e.g., completion signal) — deliver
                        // directly if metadata is present; skip empty messages otherwise.
                        if !msg.metadata.is_null() && msg.metadata != serde_json::json!({}) {
                            if let Err(e) = send_with_deadline(&**channel, &msg).await {
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
                            if let Err(e) = send_with_deadline(&**channel, &chunk_msg).await {
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

        match ready_rx.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(eyre::eyre!("outbound dispatcher failed to start: {e}")),
            Err(_) => Err(eyre::eyre!(
                "outbound dispatcher task died before signaling readiness"
            )),
        }
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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

    struct HangingChannel {
        send_started: Arc<AtomicBool>,
    }

    #[async_trait]
    impl Channel for HangingChannel {
        fn name(&self) -> &str {
            "hanging"
        }

        async fn start(&self, _inbound_tx: mpsc::Sender<InboundMessage>) -> Result<()> {
            Ok(())
        }

        async fn send(&self, _msg: &OutboundMessage) -> Result<()> {
            self.send_started.store(true, Ordering::SeqCst);
            std::future::pending::<()>().await;
            Ok(())
        }
    }

    /// A channel whose `send` never resolves must not wedge the whole outbound
    /// queue: after the per-send deadline the dispatcher must move on to the
    /// next message (R-14 head-of-line blocking).
    #[tokio::test(start_paused = true)]
    async fn should_dispatch_next_message_when_channel_send_is_stuck() {
        let hanging = Arc::new(HangingChannel {
            send_started: Arc::new(AtomicBool::new(false)),
        });
        let hanging_started = Arc::clone(&hanging.send_started);
        let fast = Arc::new(MockChannel::new("fast"));
        let fast_sent = Arc::clone(&fast.sent);

        let mut mgr = ChannelManager::new();
        mgr.register(hanging);
        mgr.register(fast);

        let (agent, publisher) = crate::bus::create_bus();
        mgr.start_all(publisher).await.unwrap();

        agent
            .send_outbound(OutboundMessage {
                channel: "hanging".into(),
                chat_id: "c1".into(),
                content: "never delivered".into(),
                reply_to: None,
                media: vec![],
                metadata: serde_json::json!({}),
            })
            .await
            .unwrap();
        while !hanging_started.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }

        agent
            .send_outbound(OutboundMessage {
                channel: "fast".into(),
                chat_id: "c1".into(),
                content: "must not queue behind".into(),
                reply_to: None,
                media: vec![],
                metadata: serde_json::json!({}),
            })
            .await
            .unwrap();

        // Give the dispatcher far more than the per-send deadline in virtual
        // time; a bounded dispatcher must have moved on by then.
        tokio::time::advance(std::time::Duration::from_secs(600)).await;
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }

        assert!(
            fast_sent
                .lock()
                .await
                .iter()
                .any(|m| m == "must not queue behind"),
            "a stuck send must time out instead of head-of-line blocking the bus"
        );
    }
}
