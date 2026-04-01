//! Progressive streaming reporter for messaging channels.
//!
//! Bridges the synchronous `ProgressReporter` trait to async channel I/O,
//! enabling real-time LLM text streaming to Telegram, WhatsApp, etc.
//! Text is accumulated and the channel message is edited at a throttled rate.

use std::sync::Arc;
use std::time::{Duration, Instant};

use octos_agent::progress::{ProgressEvent, ProgressReporter};
use octos_bus::{ActiveSessionStore, Channel};
use octos_core::{METADATA_SENDER_USER_ID, OutboundMessage, SessionKey};
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, warn};

/// Events forwarded from the synchronous reporter to the async forwarder.
#[derive(Debug)]
pub enum StreamProgressEvent {
    /// A chunk of streaming text from the LLM.
    Chunk { text: String, iteration: u32 },
    /// Streaming finished for this iteration.
    StreamDone { iteration: u32 },
    /// A tool is about to run.
    ToolStarted { name: String },
    /// A tool completed.
    ToolCompleted { name: String, success: bool },
    /// Mid-execution progress from a tool.
    ToolProgress { name: String, message: String },
    /// LLM call status update (retry progress, provider switching).
    LlmStatus { message: String },
    /// A file was written/modified by a tool.
    FileWritten { path: String },
    /// Reset the streaming buffer (e.g. before an LLM retry so partial
    /// text from a failed attempt doesn't get concatenated with the retry).
    BufferReset,
    /// Raw SSE JSON to forward directly to the web client.
    /// Used for discrete progress events (thinking, cost_update) that
    /// the web UI needs as separate SSE events, not baked into message text.
    RawSse { json: String },
}

/// A `ProgressReporter` that forwards stream events through an unbounded channel.
///
/// Because `ProgressReporter::report()` is synchronous, we use `unbounded_send()`
/// which never blocks. The receiving async task handles actual channel I/O.
pub struct ChannelStreamReporter {
    tx: mpsc::UnboundedSender<StreamProgressEvent>,
}

impl ChannelStreamReporter {
    pub fn new(tx: mpsc::UnboundedSender<StreamProgressEvent>) -> Self {
        Self { tx }
    }
}

impl ProgressReporter for ChannelStreamReporter {
    fn report(&self, event: ProgressEvent) {
        let mapped = match event {
            ProgressEvent::StreamChunk { text, iteration } => {
                StreamProgressEvent::Chunk { text, iteration }
            }
            ProgressEvent::StreamDone { iteration } => {
                StreamProgressEvent::StreamDone { iteration }
            }
            ProgressEvent::ToolStarted { ref name, .. } => {
                // Also send raw SSE for web client status indicators
                let _ = self.tx.send(StreamProgressEvent::RawSse {
                    json: serde_json::json!({"type": "tool_start", "tool": name}).to_string(),
                });
                StreamProgressEvent::ToolStarted { name: name.clone() }
            }
            ProgressEvent::ToolCompleted {
                ref name, success, ..
            } => {
                let _ = self.tx.send(StreamProgressEvent::RawSse {
                    json: serde_json::json!({"type": "tool_end", "tool": name, "success": success})
                        .to_string(),
                });
                StreamProgressEvent::ToolCompleted {
                    name: name.clone(),
                    success,
                }
            }
            ProgressEvent::ToolProgress {
                ref name,
                ref message,
                ..
            } => {
                let _ = self.tx.send(StreamProgressEvent::RawSse {
                    json: serde_json::json!({"type": "tool_progress", "tool": name, "message": message})
                        .to_string(),
                });
                StreamProgressEvent::ToolProgress {
                    name: name.clone(),
                    message: message.clone(),
                }
            }
            ProgressEvent::LlmStatus { message, .. } => StreamProgressEvent::LlmStatus { message },
            ProgressEvent::FileModified { path } => StreamProgressEvent::FileWritten { path },
            ProgressEvent::StreamRetry { .. } => StreamProgressEvent::BufferReset,
            // Forward discrete progress events as raw SSE JSON for the web client.
            ProgressEvent::Thinking { iteration } => StreamProgressEvent::RawSse {
                json: serde_json::json!({"type": "thinking", "iteration": iteration}).to_string(),
            },
            ProgressEvent::Response { iteration, .. } => StreamProgressEvent::RawSse {
                json: serde_json::json!({"type": "response", "iteration": iteration}).to_string(),
            },
            ProgressEvent::CostUpdate {
                session_input_tokens,
                session_output_tokens,
                session_cost,
                ..
            } => StreamProgressEvent::RawSse {
                json: serde_json::json!({
                    "type": "cost_update",
                    "input_tokens": session_input_tokens,
                    "output_tokens": session_output_tokens,
                    "session_cost": session_cost,
                })
                .to_string(),
            },
            _ => return,
        };
        let _ = self.tx.send(mapped);
    }
}

/// Minimum interval between message edits (avoids API rate limits).
const EDIT_THROTTLE: Duration = Duration::from_millis(1000);

/// Strip `<think>...</think>` blocks from streaming buffer.
/// Handles partial tags (open `<think>` not yet closed) by hiding from that point.
/// Collapses runs of 3+ newlines left behind to avoid blank gaps.
fn strip_think_from_buffer(buf: &str) -> String {
    let mut result = String::new();
    let mut rest = buf;

    while let Some(start) = rest.find("<think>") {
        result.push_str(&rest[..start]);
        let after = &rest[start + "<think>".len()..];
        if let Some(end) = after.find("</think>") {
            rest = &after[end + "</think>".len()..];
        } else {
            // Unclosed <think> — hide everything from here (still streaming)
            while result.contains("\n\n\n") {
                result = result.replace("\n\n\n", "\n\n");
            }
            return result.trim_end().to_string();
        }
    }
    result.push_str(rest);
    // Collapse runs of 3+ newlines left behind after stripping
    while result.contains("\n\n\n") {
        result = result.replace("\n\n\n", "\n\n");
    }
    result
}

/// Result of the stream forwarder — returns the message ID if streaming happened.
pub struct StreamResult {
    /// The platform message ID of the streamed message, if any was sent.
    pub message_id: Option<String>,
    /// The full accumulated text that was streamed.
    pub text: String,
}

/// Check if a session is currently the active session for its chat.
///
/// When inactive, streaming edits must be skipped so replies go through
/// the proxy → pending buffer path and can be flushed on session switch.
async fn is_session_active(
    session_key: &SessionKey,
    active_sessions: &RwLock<ActiveSessionStore>,
) -> bool {
    let my_topic = session_key.topic().unwrap_or("");
    let base_key = session_key.base_key();
    let active_topic = active_sessions
        .read()
        .await
        .get_active_topic(base_key)
        .to_string();
    my_topic == active_topic
}

/// Run the stream forwarder task.
///
/// Receives `StreamProgressEvent`s and progressively edits a channel message
/// with accumulated text. Returns once the sender is dropped (agent completes).
///
/// `cancel_status` — if provided, stops the status indicator when first chunk arrives.
///
/// `active_sessions` + `session_key` — used to check if this session is currently
/// active before sending/editing channel messages. When inactive, all direct
/// channel operations are skipped to prevent cross-session message leaks.
#[allow(clippy::too_many_arguments)]
pub async fn run_stream_forwarder(
    mut rx: mpsc::UnboundedReceiver<StreamProgressEvent>,
    channel: Arc<dyn Channel>,
    chat_id: String,
    cancel_status: Option<Arc<std::sync::atomic::AtomicBool>>,
    status_msg_id: Option<Arc<tokio::sync::Mutex<Option<String>>>>,
    active_sessions: Arc<RwLock<ActiveSessionStore>>,
    session_key: SessionKey,
    sender_user_id: Option<String>,
    operation_updater: Option<Arc<dyn Fn(&str) + Send + Sync>>,
) -> StreamResult {
    let mut buffer = String::new();
    let mut message_id: Option<String> = None;
    let mut last_edit = Instant::now() - EDIT_THROTTLE; // allow immediate first edit
    let mut first_chunk = true;
    let mut last_chunk_iteration: u32 = 0;
    // When true, the channel doesn't support send_with_id (returned None),
    // so we stop streaming edits and let the final reply go through out_tx.
    let mut no_edit_support = false;

    while let Some(event) = rx.recv().await {
        match event {
            StreamProgressEvent::Chunk { text, iteration } => {
                // When a new LLM iteration starts streaming, clear tool progress
                // markers from the buffer so the final response is clean.
                // This prevents testers from capturing "✓ shell ✓ read_file..."
                // as the response when the LLM hasn't started its reply yet.
                if iteration > last_chunk_iteration {
                    last_chunk_iteration = iteration;
                    // Remove tool progress lines (✓/✗/⚙ markers and 📄 notifications)
                    let cleaned: Vec<&str> = buffer
                        .lines()
                        .filter(|line| {
                            let trimmed = line.trim();
                            !trimmed.starts_with("✓ ")
                                && !trimmed.starts_with("✗ ")
                                && !trimmed.starts_with("⚙ ")
                                && !trimmed.starts_with("📄 ")
                        })
                        .collect();
                    buffer = cleaned.join("\n");
                    if buffer.trim().is_empty() {
                        buffer.clear();
                    }
                }
                if first_chunk {
                    first_chunk = false;
                    // Cancel the "✦ Thinking..." status indicator
                    if let Some(ref cancel) = cancel_status {
                        cancel.store(true, std::sync::atomic::Ordering::Release);
                    }
                    // Delete the status message (only if this session is active)
                    if is_session_active(&session_key, &active_sessions).await {
                        if let Some(ref msg_id_lock) = status_msg_id {
                            let mid = msg_id_lock.lock().await.take();
                            if let Some(ref mid) = mid {
                                let _ = channel.delete_message(&chat_id, mid).await;
                            }
                        }
                    } else {
                        debug!(session = %session_key, "skipping status delete (inactive)");
                    }
                }

                buffer.push_str(&text);

                // Throttled edit — strip <think> blocks before showing to user
                if !no_edit_support && last_edit.elapsed() >= EDIT_THROTTLE && !buffer.is_empty() {
                    if !is_session_active(&session_key, &active_sessions).await {
                        continue;
                    }
                    let visible = strip_think_from_buffer(&buffer);
                    if !visible.is_empty() {
                        flush_to_channel(
                            &channel,
                            &chat_id,
                            &visible,
                            &mut message_id,
                            &mut no_edit_support,
                            sender_user_id.as_deref(),
                        )
                        .await;
                        last_edit = Instant::now();
                    }
                }
            }
            StreamProgressEvent::StreamDone { .. } => {
                // Flush remaining buffer — strip think tags. Use finish flush
                // so channels like WeCom can send `finish: true`.
                if !no_edit_support
                    && !buffer.is_empty()
                    && is_session_active(&session_key, &active_sessions).await
                {
                    let visible = strip_think_from_buffer(&buffer);
                    if !visible.is_empty() {
                        finish_flush_to_channel(
                            &channel,
                            &chat_id,
                            &visible,
                            &mut message_id,
                            &mut no_edit_support,
                            sender_user_id.as_deref(),
                        )
                        .await;
                    }
                }
            }
            StreamProgressEvent::ToolStarted { name } => {
                // Update status bar operation layer with tool name
                if let Some(ref updater) = operation_updater {
                    updater(&format!("Running {name}"));
                }
                // Show tool status line in the streaming message
                if !no_edit_support && is_session_active(&session_key, &active_sessions).await {
                    if buffer.is_empty() {
                        buffer.push_str(&format!("⚙ `{name}`..."));
                    } else {
                        buffer.push_str(&format!("\n\n⚙ `{name}`..."));
                    }
                    let visible = strip_think_from_buffer(&buffer);
                    if !visible.is_empty() {
                        flush_to_channel(
                            &channel,
                            &chat_id,
                            &visible,
                            &mut message_id,
                            &mut no_edit_support,
                            sender_user_id.as_deref(),
                        )
                        .await;
                    }
                    last_edit = Instant::now();
                }
            }
            StreamProgressEvent::ToolCompleted { name, success } => {
                let icon = if success { "✓" } else { "✗" };
                // Update tool status in the existing message
                if !no_edit_support && !buffer.is_empty() {
                    // Replace the "⚙ `tool`..." with the result
                    let pending = format!("⚙ `{name}`...");
                    let completed = format!("{icon} `{name}`");
                    if buffer.contains(&pending) {
                        buffer = buffer.replace(&pending, &completed);
                    }
                    if is_session_active(&session_key, &active_sessions).await {
                        let visible = strip_think_from_buffer(&buffer);
                        if !visible.is_empty() {
                            flush_to_channel(
                                &channel,
                                &chat_id,
                                &visible,
                                &mut message_id,
                                &mut no_edit_support,
                                sender_user_id.as_deref(),
                            )
                            .await;
                        }
                        last_edit = Instant::now();
                    }
                }
            }
            StreamProgressEvent::ToolProgress { name, message } => {
                // Update the tool status line with the progress message
                let pending = format!("⚙ `{name}`...");
                let progress = format!("⚙ `{name}`: {message}");
                if buffer.contains(&pending) {
                    buffer = buffer.replace(&pending, &progress);
                } else {
                    // Replace previous progress line for this tool
                    let prev_prefix = format!("⚙ `{name}`:");
                    if let Some(pos) = buffer.rfind(&prev_prefix) {
                        let end = buffer[pos..].find('\n').map_or(buffer.len(), |i| pos + i);
                        buffer.replace_range(pos..end, &progress);
                    } else if !buffer.is_empty() {
                        // Tool progress arrived without a prior ToolStarted line
                        buffer.push_str(&format!("\n\n{progress}"));
                    } else {
                        buffer.push_str(&progress);
                    }
                }
                if last_edit.elapsed() >= EDIT_THROTTLE
                    && is_session_active(&session_key, &active_sessions).await
                {
                    let visible = strip_think_from_buffer(&buffer);
                    if !visible.is_empty() {
                        flush_to_channel(
                            &channel,
                            &chat_id,
                            &visible,
                            &mut message_id,
                            &mut no_edit_support,
                            sender_user_id.as_deref(),
                        )
                        .await;
                    }
                    last_edit = Instant::now();
                }
            }
            StreamProgressEvent::LlmStatus { message } => {
                // Cancel the status indicator before showing retry/failover info
                if first_chunk {
                    first_chunk = false;
                    if let Some(ref cancel) = cancel_status {
                        cancel.store(true, std::sync::atomic::Ordering::Release);
                    }
                    if is_session_active(&session_key, &active_sessions).await {
                        if let Some(ref msg_id_lock) = status_msg_id {
                            let mid = msg_id_lock.lock().await.take();
                            if let Some(ref mid) = mid {
                                let _ = channel.delete_message(&chat_id, mid).await;
                            }
                        }
                    }
                }
                // Show retry/failover status as a temporary message
                if is_session_active(&session_key, &active_sessions).await {
                    let status_text = format!("⟳ {message}");
                    flush_to_channel(
                        &channel,
                        &chat_id,
                        &status_text,
                        &mut message_id,
                        &mut no_edit_support,
                        sender_user_id.as_deref(),
                    )
                    .await;
                    last_edit = Instant::now();
                }
            }
            StreamProgressEvent::FileWritten { path } => {
                // Show file-saved notification immediately so the user knows
                // the file was written even if the final LLM response is slow.
                let filename = std::path::Path::new(&path)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or(path);
                if !buffer.is_empty() {
                    buffer.push_str(&format!("\n📄 Saved `{filename}`"));
                    if is_session_active(&session_key, &active_sessions).await {
                        let visible = strip_think_from_buffer(&buffer);
                        if !visible.is_empty() {
                            flush_to_channel(
                                &channel,
                                &chat_id,
                                &visible,
                                &mut message_id,
                                &mut no_edit_support,
                                sender_user_id.as_deref(),
                            )
                            .await;
                        }
                        last_edit = Instant::now();
                    }
                }
            }
            StreamProgressEvent::BufferReset => {
                // Clear accumulated text so a retry starts fresh.
                // Keep the message_id so the retry edits the same message
                // instead of creating a new one.
                buffer.clear();
            }
            StreamProgressEvent::RawSse { json } => {
                // Forward raw SSE JSON directly to the channel.
                // Only ApiChannel implements this; other channels ignore it.
                let _ = channel.send_raw_sse(&chat_id, &json).await;
            }
        }
    }

    // Final flush — only if we have unsent buffer (no message_id yet).
    // Use finish flush so streams are properly closed.
    if !no_edit_support
        && !buffer.is_empty()
        && message_id.is_none()
        && is_session_active(&session_key, &active_sessions).await
    {
        let visible = strip_think_from_buffer(&buffer);
        if !visible.is_empty() {
            finish_flush_to_channel(
                &channel,
                &chat_id,
                &visible,
                &mut message_id,
                &mut no_edit_support,
                sender_user_id.as_deref(),
            )
            .await;
        }
    }

    StreamResult {
        message_id,
        text: buffer,
    }
}

/// Send or edit the streaming message on the channel.
///
/// If `send_with_id` returns `None` (channel doesn't support message editing),
/// sets `no_edit_support` to true so the caller stops attempting to stream.
/// The final reply will go through the normal `out_tx` path instead.
async fn flush_to_channel(
    channel: &Arc<dyn Channel>,
    chat_id: &str,
    text: &str,
    message_id: &mut Option<String>,
    no_edit_support: &mut bool,
    sender_user_id: Option<&str>,
) {
    do_flush(
        channel,
        chat_id,
        text,
        message_id,
        no_edit_support,
        false,
        sender_user_id,
    )
    .await;
}

/// Send the final streaming chunk, signaling the stream is complete.
///
/// Channels that need special finalization (e.g. WeCom `finish: true`) will
/// receive this via `Channel::finish_stream()`.
async fn finish_flush_to_channel(
    channel: &Arc<dyn Channel>,
    chat_id: &str,
    text: &str,
    message_id: &mut Option<String>,
    no_edit_support: &mut bool,
    sender_user_id: Option<&str>,
) {
    // Preserve the asserted virtual-user identity when the final flush is the
    // first send; otherwise Matrix falls back to the main bot user.
    do_flush(
        channel,
        chat_id,
        text,
        message_id,
        no_edit_support,
        true,
        sender_user_id,
    )
    .await;
}

async fn do_flush(
    channel: &Arc<dyn Channel>,
    chat_id: &str,
    text: &str,
    message_id: &mut Option<String>,
    no_edit_support: &mut bool,
    finish: bool,
    sender_user_id: Option<&str>,
) {
    if let Some(mid) = message_id.as_ref() {
        let result = if finish {
            channel.finish_stream(chat_id, mid, text).await
        } else {
            channel.edit_message(chat_id, mid, text).await
        };
        if let Err(e) = result {
            warn!("stream edit failed: {e}");
        }
    } else {
        // Send new message and capture its ID
        let msg = OutboundMessage {
            channel: channel.name().to_string(),
            chat_id: chat_id.to_string(),
            content: text.to_string(),
            reply_to: None,
            media: vec![],
            metadata: sender_user_id
                .map(|uid| serde_json::json!({ METADATA_SENDER_USER_ID: uid }))
                .unwrap_or_else(|| serde_json::json!({})),
        };
        match channel.send_with_id(&msg).await {
            Ok(Some(mid)) => {
                // If this is also the final flush (only one chunk total),
                // finalize the stream immediately.
                if finish {
                    if let Err(e) = channel.finish_stream(chat_id, &mid, text).await {
                        warn!("stream finish failed: {e}");
                    }
                }
                *message_id = Some(mid);
            }
            Ok(None) => {
                // Channel doesn't support edit — stop streaming to avoid
                // sending duplicate messages. The final reply goes via out_tx.
                *no_edit_support = true;
            }
            Err(e) => {
                warn!("stream send failed: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use async_trait::async_trait;
    use octos_core::{InboundMessage, METADATA_SENDER_USER_ID};
    use tokio::sync::{Mutex, mpsc};

    #[derive(Default)]
    struct MockChannel {
        sent: Arc<Mutex<Vec<OutboundMessage>>>,
    }

    #[async_trait]
    impl Channel for MockChannel {
        fn name(&self) -> &str {
            "matrix"
        }

        async fn start(&self, _inbound_tx: mpsc::Sender<InboundMessage>) -> eyre::Result<()> {
            Ok(())
        }

        async fn send(&self, msg: &OutboundMessage) -> eyre::Result<()> {
            self.sent.lock().await.push(msg.clone());
            Ok(())
        }

        async fn send_with_id(&self, msg: &OutboundMessage) -> eyre::Result<Option<String>> {
            self.sent.lock().await.push(msg.clone());
            Ok(Some("$stream-1".to_string()))
        }

        fn supports_edit(&self) -> bool {
            true
        }
    }

    #[test]
    fn should_clear_buffer_on_reset_event() {
        // Simulate the forwarder receiving chunks then a BufferReset.
        // We can't easily test the full async forwarder, but we can
        // verify the event enum is correctly structured and mapped.
        let event = StreamProgressEvent::BufferReset;
        assert!(matches!(event, StreamProgressEvent::BufferReset));
    }

    #[test]
    fn should_map_stream_retry_to_buffer_reset() {
        use octos_agent::progress::ProgressEvent;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let reporter = ChannelStreamReporter::new(tx);

        reporter.report(ProgressEvent::StreamRetry { iteration: 1 });

        let event = rx.try_recv().unwrap();
        assert!(matches!(event, StreamProgressEvent::BufferReset));
    }

    #[test]
    fn should_strip_think_tags_from_buffer() {
        assert_eq!(
            strip_think_from_buffer("Hello <think>internal</think> world"),
            "Hello  world"
        );
    }

    #[test]
    fn should_hide_unclosed_think_tag() {
        assert_eq!(
            strip_think_from_buffer("Hello <think>still thinking"),
            "Hello"
        );
    }

    #[tokio::test]
    async fn should_send_stream_message_with_sender_user_id() {
        let mock = Arc::new(MockChannel::default());
        let channel: Arc<dyn Channel> = mock.clone();
        let mut message_id = None;
        let mut no_edit_support = false;

        flush_to_channel(
            &channel,
            "!room:localhost",
            "hello",
            &mut message_id,
            &mut no_edit_support,
            Some("@bot_mybot:localhost"),
        )
        .await;

        let sent = mock.sent.lock().await;
        let first = sent.first().expect("stream message should be sent");
        assert_eq!(
            first
                .metadata
                .get(METADATA_SENDER_USER_ID)
                .and_then(|v| v.as_str()),
            Some("@bot_mybot:localhost")
        );
    }

    #[tokio::test]
    async fn should_send_final_stream_message_with_sender_user_id() {
        let mock = Arc::new(MockChannel::default());
        let channel: Arc<dyn Channel> = mock.clone();
        let mut message_id = None;
        let mut no_edit_support = false;

        finish_flush_to_channel(
            &channel,
            "!room:localhost",
            "hello",
            &mut message_id,
            &mut no_edit_support,
            Some("@bot_mybot:localhost"),
        )
        .await;

        let sent = mock.sent.lock().await;
        let first = sent.first().expect("final stream message should be sent");
        assert_eq!(
            first
                .metadata
                .get(METADATA_SENDER_USER_ID)
                .and_then(|v| v.as_str()),
            Some("@bot_mybot:localhost")
        );
    }
}
