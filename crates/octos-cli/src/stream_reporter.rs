//! Progressive streaming reporter for messaging channels.
//!
//! Bridges the synchronous `ProgressReporter` trait to async channel I/O,
//! enabling real-time LLM text streaming to Telegram, WhatsApp, etc.
//! Text is accumulated and the channel message is edited at a throttled rate.

use std::sync::Arc;
use std::time::{Duration, Instant};

use octos_agent::progress::{ProgressEvent, ProgressReporter};
use octos_bus::{ActiveSessionStore, Channel};
use octos_core::{OutboundMessage, SessionKey};
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
    /// Reset the streaming buffer (e.g. before an LLM retry so partial
    /// text from a failed attempt doesn't get concatenated with the retry).
    BufferReset,
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
            ProgressEvent::ToolStarted { name, .. } => StreamProgressEvent::ToolStarted { name },
            ProgressEvent::ToolCompleted { name, success, .. } => {
                StreamProgressEvent::ToolCompleted { name, success }
            }
            ProgressEvent::ToolProgress { name, message, .. } => {
                StreamProgressEvent::ToolProgress { name, message }
            }
            ProgressEvent::LlmStatus { message, .. } => StreamProgressEvent::LlmStatus { message },
            ProgressEvent::StreamRetry { .. } => StreamProgressEvent::BufferReset,
            _ => return,
        };
        let _ = self.tx.send(mapped);
    }
}

/// Minimum interval between message edits (avoids API rate limits).
const EDIT_THROTTLE: Duration = Duration::from_millis(1000);

/// Strip `<think>...</think>` blocks from streaming buffer.
/// Handles partial tags (open `<think>` not yet closed) by hiding from that point.
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
            return result.trim_end().to_string();
        }
    }
    result.push_str(rest);
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
pub async fn run_stream_forwarder(
    mut rx: mpsc::UnboundedReceiver<StreamProgressEvent>,
    channel: Arc<dyn Channel>,
    chat_id: String,
    cancel_status: Option<Arc<std::sync::atomic::AtomicBool>>,
    status_msg_id: Option<Arc<tokio::sync::Mutex<Option<String>>>>,
    active_sessions: Arc<RwLock<ActiveSessionStore>>,
    session_key: SessionKey,
) -> StreamResult {
    let mut buffer = String::new();
    let mut message_id: Option<String> = None;
    let mut last_edit = Instant::now() - EDIT_THROTTLE; // allow immediate first edit
    let mut first_chunk = true;
    // When true, the channel doesn't support send_with_id (returned None),
    // so we stop streaming edits and let the final reply go through out_tx.
    let mut no_edit_support = false;

    while let Some(event) = rx.recv().await {
        match event {
            StreamProgressEvent::Chunk { text, .. } => {
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
                        )
                        .await;
                    }
                }
            }
            StreamProgressEvent::ToolStarted { name } => {
                // Flush text before tool status
                if !no_edit_support
                    && !buffer.is_empty()
                    && is_session_active(&session_key, &active_sessions).await
                {
                    buffer.push_str(&format!("\n\n⚙ `{name}`..."));
                    flush_to_channel(
                        &channel,
                        &chat_id,
                        &buffer,
                        &mut message_id,
                        &mut no_edit_support,
                    )
                    .await;
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
                        flush_to_channel(
                            &channel,
                            &chat_id,
                            &buffer,
                            &mut message_id,
                            &mut no_edit_support,
                        )
                        .await;
                        last_edit = Instant::now();
                    }
                }
            }
            StreamProgressEvent::ToolProgress { name, message } => {
                // Update the tool status line with the progress message
                if !buffer.is_empty() {
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
                        }
                    }
                    if last_edit.elapsed() >= EDIT_THROTTLE
                        && is_session_active(&session_key, &active_sessions).await
                    {
                        flush_to_channel(
                            &channel,
                            &chat_id,
                            &buffer,
                            &mut message_id,
                            &mut no_edit_support,
                        )
                        .await;
                        last_edit = Instant::now();
                    }
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
                    )
                    .await;
                    last_edit = Instant::now();
                }
            }
            StreamProgressEvent::BufferReset => {
                // Clear accumulated text so a retry starts fresh.
                // Keep the message_id so the retry edits the same message
                // instead of creating a new one.
                buffer.clear();
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
) {
    do_flush(channel, chat_id, text, message_id, no_edit_support, false).await;
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
) {
    do_flush(channel, chat_id, text, message_id, no_edit_support, true).await;
}

async fn do_flush(
    channel: &Arc<dyn Channel>,
    chat_id: &str,
    text: &str,
    message_id: &mut Option<String>,
    no_edit_support: &mut bool,
    finish: bool,
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
            metadata: serde_json::json!({}),
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
}
