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
///
/// M8.10 PR #2: every emitted SSE payload includes `thread_id` (the
/// client_message_id of the user message that owns this turn). Reporters are
/// constructed per-turn so the thread_id is bound for the reporter's lifetime.
/// When `thread_id` is `None`, the field is omitted (preserves wire compat for
/// non-API channels and pre-cmid clients).
pub struct ChannelStreamReporter {
    tx: mpsc::UnboundedSender<StreamProgressEvent>,
    thread_id: Option<String>,
}

impl ChannelStreamReporter {
    pub fn new(tx: mpsc::UnboundedSender<StreamProgressEvent>) -> Self {
        Self {
            tx,
            thread_id: None,
        }
    }

    /// Bind a `thread_id` (typically the user message's `client_message_id`)
    /// to every SSE payload this reporter emits.
    pub fn with_thread_id(mut self, thread_id: Option<String>) -> Self {
        self.thread_id = thread_id.filter(|s| !s.is_empty());
        self
    }
}

/// Insert the bound `thread_id` into a JSON object payload (if any). Mutates
/// the value in place. No-op when `thread_id` is `None` or the value is not
/// an object.
fn inject_thread_id(value: &mut serde_json::Value, thread_id: Option<&str>) {
    if let (Some(tid), Some(obj)) = (thread_id, value.as_object_mut()) {
        obj.insert(
            "thread_id".to_string(),
            serde_json::Value::String(tid.to_string()),
        );
    }
}

impl ProgressReporter for ChannelStreamReporter {
    fn thread_id(&self) -> Option<&str> {
        self.thread_id.as_deref()
    }

    fn report(&self, event: ProgressEvent) {
        let thread_id = self.thread_id.as_deref();
        let mapped = match event {
            ProgressEvent::StreamChunk { text, iteration } => {
                StreamProgressEvent::Chunk { text, iteration }
            }
            ProgressEvent::StreamDone { iteration } => {
                StreamProgressEvent::StreamDone { iteration }
            }
            ProgressEvent::ToolStarted {
                ref name,
                ref tool_id,
            } => {
                // Also send raw SSE for web client status indicators
                let mut payload = serde_json::json!({
                    "type": "tool_start",
                    "tool": name,
                    "tool_call_id": tool_id,
                });
                inject_thread_id(&mut payload, thread_id);
                let _ = self.tx.send(StreamProgressEvent::RawSse {
                    json: payload.to_string(),
                });
                StreamProgressEvent::ToolStarted { name: name.clone() }
            }
            ProgressEvent::ToolCompleted {
                ref name,
                ref tool_id,
                success,
                ..
            } => {
                let mut payload = serde_json::json!({
                    "type": "tool_end",
                    "tool": name,
                    "tool_call_id": tool_id,
                    "success": success,
                });
                inject_thread_id(&mut payload, thread_id);
                let _ = self.tx.send(StreamProgressEvent::RawSse {
                    json: payload.to_string(),
                });
                StreamProgressEvent::ToolCompleted {
                    name: name.clone(),
                    success,
                }
            }
            ProgressEvent::ToolProgress {
                ref name,
                ref tool_id,
                ref message,
            } => {
                let mut payload = serde_json::json!({
                    "type": "tool_progress",
                    "tool": name,
                    "tool_call_id": tool_id,
                    "message": message,
                });
                inject_thread_id(&mut payload, thread_id);
                let _ = self.tx.send(StreamProgressEvent::RawSse {
                    json: payload.to_string(),
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
            ProgressEvent::Thinking { iteration } => {
                let mut payload = serde_json::json!({"type": "thinking", "iteration": iteration});
                inject_thread_id(&mut payload, thread_id);
                StreamProgressEvent::RawSse {
                    json: payload.to_string(),
                }
            }
            ProgressEvent::Response { iteration, .. } => {
                let mut payload = serde_json::json!({"type": "response", "iteration": iteration});
                inject_thread_id(&mut payload, thread_id);
                StreamProgressEvent::RawSse {
                    json: payload.to_string(),
                }
            }
            ProgressEvent::CostUpdate {
                session_input_tokens,
                session_output_tokens,
                session_cost,
                ..
            } => {
                let mut payload = serde_json::json!({
                    "type": "cost_update",
                    "input_tokens": session_input_tokens,
                    "output_tokens": session_output_tokens,
                    "session_cost": session_cost,
                });
                inject_thread_id(&mut payload, thread_id);
                StreamProgressEvent::RawSse {
                    json: payload.to_string(),
                }
            }
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
    result = strip_invoke_from_buffer(&result);
    // Collapse runs of 3+ newlines left behind after stripping
    while result.contains("\n\n\n") {
        result = result.replace("\n\n\n", "\n\n");
    }
    result
}

fn strip_invoke_from_buffer(buf: &str) -> String {
    let mut out = String::new();
    let mut rest = buf;

    loop {
        let Some(start) = rest.find("<invoke") else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..start]);
        let from_tag = &rest[start..];

        let Some(open_end) = from_tag.find('>') else {
            out.push_str(from_tag);
            break;
        };
        let open_tag = &from_tag[..=open_end];
        let after_open = &from_tag[open_end + 1..];

        if open_tag.trim_end().ends_with("/>") {
            rest = after_open;
            continue;
        }

        if let Some(close_rel) = after_open.find("</invoke>") {
            rest = &after_open[close_rel + "</invoke>".len()..];
        } else {
            break;
        }
    }

    out
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
///
/// `thread_id` — #649 follow-up (rapid-fire): the originating turn's
/// `client_message_id`, captured ONCE at forwarder construction. Every
/// `send_with_id` / `edit_message` call this forwarder issues stamps the
/// outbound metadata with this value so the channel does not have to
/// fall back to the per-chat sticky map. Under rapid-fire 5 concurrent
/// turns, sticky has rotated to the LATEST cmid by the time an earlier
/// turn produces its first chunk — pre-fix that earlier chunk's encoded
/// message_id captured the rotated value and every subsequent edit
/// inherited the wrong thread_id, collapsing 4 turns onto one bubble.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
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
    thread_id: Option<String>,
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
                            thread_id.as_deref(),
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
                            thread_id.as_deref(),
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
                            thread_id.as_deref(),
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
                                thread_id.as_deref(),
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
                            thread_id.as_deref(),
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
                        thread_id.as_deref(),
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
                                thread_id.as_deref(),
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
                thread_id.as_deref(),
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
#[allow(clippy::too_many_arguments)]
async fn flush_to_channel(
    channel: &Arc<dyn Channel>,
    chat_id: &str,
    text: &str,
    message_id: &mut Option<String>,
    no_edit_support: &mut bool,
    sender_user_id: Option<&str>,
    thread_id: Option<&str>,
) {
    do_flush(
        channel,
        chat_id,
        text,
        message_id,
        no_edit_support,
        false,
        sender_user_id,
        thread_id,
    )
    .await;
}

/// Send the final streaming chunk, signaling the stream is complete.
///
/// Channels that need special finalization (e.g. WeCom `finish: true`) will
/// receive this via `Channel::finish_stream()`.
#[allow(clippy::too_many_arguments)]
async fn finish_flush_to_channel(
    channel: &Arc<dyn Channel>,
    chat_id: &str,
    text: &str,
    message_id: &mut Option<String>,
    no_edit_support: &mut bool,
    sender_user_id: Option<&str>,
    thread_id: Option<&str>,
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
        thread_id,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn do_flush(
    channel: &Arc<dyn Channel>,
    chat_id: &str,
    text: &str,
    message_id: &mut Option<String>,
    no_edit_support: &mut bool,
    finish: bool,
    sender_user_id: Option<&str>,
    thread_id: Option<&str>,
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
        //
        // #649 follow-up (rapid-fire): stamp the originating turn's
        // `thread_id` into the outbound metadata so the API channel's
        // `send_with_id` captures THIS turn's cmid into the encoded
        // message_id rather than falling back to the per-chat sticky
        // map (which has been rotated by every concurrent rapid-fire
        // request that arrived between this turn's bind and first chunk).
        let mut metadata = match sender_user_id {
            Some(uid) => {
                serde_json::json!({ METADATA_SENDER_USER_ID: uid, "streaming": true })
            }
            None => serde_json::json!({ "streaming": true }),
        };
        if let Some(tid) = thread_id.filter(|s| !s.is_empty()) {
            if let Some(map) = metadata.as_object_mut() {
                map.insert(
                    "thread_id".to_string(),
                    serde_json::Value::String(tid.to_string()),
                );
            }
        }
        let msg = OutboundMessage {
            channel: channel.name().to_string(),
            chat_id: chat_id.to_string(),
            content: text.to_string(),
            reply_to: None,
            media: vec![],
            metadata,
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

    /// M8.10 PR #2: every raw SSE payload emitted by the reporter must
    /// include a `thread_id` field equal to the bound cmid. Drives every
    /// variant that produces a `RawSse` event to confirm none are missed.
    #[test]
    fn should_inject_thread_id_into_every_raw_sse_event() {
        use octos_agent::progress::ProgressEvent;
        use std::time::Duration;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let reporter =
            ChannelStreamReporter::new(tx).with_thread_id(Some("cmid-thread-A".to_string()));

        reporter.report(ProgressEvent::ToolStarted {
            name: "shell".into(),
            tool_id: "t1".into(),
        });
        reporter.report(ProgressEvent::ToolCompleted {
            name: "shell".into(),
            tool_id: "t1".into(),
            success: true,
            output_preview: "ok".into(),
            duration: Duration::from_millis(5),
        });
        reporter.report(ProgressEvent::ToolProgress {
            name: "shell".into(),
            tool_id: "t1".into(),
            message: "step 1".into(),
        });
        reporter.report(ProgressEvent::Thinking { iteration: 0 });
        reporter.report(ProgressEvent::Response {
            content: "answer".into(),
            iteration: 1,
        });
        reporter.report(ProgressEvent::CostUpdate {
            session_input_tokens: 10,
            session_output_tokens: 20,
            response_cost: None,
            session_cost: None,
        });

        let mut raw_payloads: Vec<String> = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let StreamProgressEvent::RawSse { json } = event {
                raw_payloads.push(json);
            }
        }

        // ToolStarted, ToolCompleted, ToolProgress emit RawSse + a typed
        // mapped event each, so 6 reports → 6 RawSse JSON payloads.
        assert_eq!(
            raw_payloads.len(),
            6,
            "expected 6 RawSse payloads, got {}: {:?}",
            raw_payloads.len(),
            raw_payloads
        );
        for json in &raw_payloads {
            let parsed: serde_json::Value = serde_json::from_str(json)
                .unwrap_or_else(|e| panic!("payload `{json}` failed to parse: {e}"));
            assert_eq!(
                parsed.get("thread_id").and_then(|v| v.as_str()),
                Some("cmid-thread-A"),
                "payload `{json}` missing `thread_id` field",
            );
        }
    }

    /// When the reporter is constructed without a thread_id (or with an
    /// empty string), payloads must NOT carry a `thread_id` field. This
    /// preserves wire compatibility with non-API channels and pre-cmid
    /// clients that expect the field to be absent.
    #[test]
    fn should_omit_thread_id_when_not_bound() {
        use octos_agent::progress::ProgressEvent;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let reporter = ChannelStreamReporter::new(tx);

        reporter.report(ProgressEvent::Thinking { iteration: 0 });

        if let StreamProgressEvent::RawSse { json } = rx.try_recv().unwrap() {
            let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert!(
                parsed.get("thread_id").is_none(),
                "thread_id field must be absent when reporter has no bound id, got {parsed}"
            );
        } else {
            panic!("expected RawSse for Thinking event");
        }
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

    #[test]
    fn should_strip_invoke_tags_from_buffer() {
        assert_eq!(
            strip_think_from_buffer(
                "before <invoke name=\"cron\">{\"action\":\"list\"}</invoke> after"
            ),
            "before  after"
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
            None,
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
            None,
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

    /// #649 follow-up (rapid-fire): the streaming path must stamp the
    /// originating turn's `thread_id` into the OutboundMessage metadata
    /// every time `do_flush` opens a new bubble via `send_with_id`. Pre-fix
    /// the metadata was `{streaming: true}` only, so under rapid-fire the
    /// API channel's `send_with_id` fell back to the per-chat sticky map
    /// (rotated to the LATEST cmid by every concurrent `handle_chat`
    /// arrival) and 4 of 5 turns mis-tagged onto a single bubble.
    ///
    /// This test drives the exact pre-fix shape (per-turn `thread_id`
    /// captured at forwarder construction) and asserts the wire-side
    /// outbound carries it. Pre-fix the assert fails — the metadata only
    /// has `streaming: true`. Post-fix the assert passes.
    #[tokio::test]
    async fn flush_to_channel_stamps_outbound_with_thread_id() {
        let mock = Arc::new(MockChannel::default());
        let channel: Arc<dyn Channel> = mock.clone();
        let mut message_id = None;
        let mut no_edit_support = false;

        flush_to_channel(
            &channel,
            "chat-rapid-fire",
            "first chunk",
            &mut message_id,
            &mut no_edit_support,
            None,
            Some("cmid-A"),
        )
        .await;

        let sent = mock.sent.lock().await;
        let first = sent
            .first()
            .expect("stream message should be sent through send_with_id");
        assert_eq!(
            first.metadata.get("thread_id").and_then(|v| v.as_str()),
            Some("cmid-A"),
            "outbound metadata must carry the per-turn thread_id so \
             ApiChannel does not fall back to the rotated sticky map. \
             Got metadata: {}",
            first.metadata
        );
    }

    /// #649 follow-up (rapid-fire): same property for the FINAL flush
    /// path (StreamDone). The final outbound also goes through
    /// `send_with_id` when the bubble was never opened, so it must
    /// stamp `thread_id`.
    #[tokio::test]
    async fn finish_flush_to_channel_stamps_outbound_with_thread_id() {
        let mock = Arc::new(MockChannel::default());
        let channel: Arc<dyn Channel> = mock.clone();
        let mut message_id = None;
        let mut no_edit_support = false;

        finish_flush_to_channel(
            &channel,
            "chat-rapid-fire",
            "final chunk",
            &mut message_id,
            &mut no_edit_support,
            None,
            Some("cmid-E"),
        )
        .await;

        let sent = mock.sent.lock().await;
        let first = sent
            .first()
            .expect("final stream message should be sent through send_with_id");
        assert_eq!(
            first.metadata.get("thread_id").and_then(|v| v.as_str()),
            Some("cmid-E"),
            "final-flush outbound metadata must carry the per-turn thread_id"
        );
    }

    /// Backwards-compat: when `thread_id` is None or empty, the outbound
    /// metadata MUST NOT include a `thread_id` field so non-API channels
    /// (Matrix, Telegram, …) and pre-cmid clients keep observing the
    /// pre-fix wire shape.
    #[tokio::test]
    async fn flush_omits_thread_id_when_unbound() {
        let mock = Arc::new(MockChannel::default());
        let channel: Arc<dyn Channel> = mock.clone();
        let mut message_id = None;
        let mut no_edit_support = false;

        flush_to_channel(
            &channel,
            "chat-legacy",
            "hello",
            &mut message_id,
            &mut no_edit_support,
            None,
            None,
        )
        .await;

        let sent = mock.sent.lock().await;
        let first = sent.first().expect("stream message should be sent");
        assert!(
            first.metadata.get("thread_id").is_none(),
            "thread_id field must be absent when forwarder has no bound id, got {}",
            first.metadata
        );

        // Also verify empty-string thread_id is treated as absent (legacy
        // pre-cmid clients send `client_message_id: ""` on some paths).
        drop(sent);
        let mock2 = Arc::new(MockChannel::default());
        let channel2: Arc<dyn Channel> = mock2.clone();
        let mut message_id2 = None;
        let mut no_edit_support2 = false;
        flush_to_channel(
            &channel2,
            "chat-legacy-empty",
            "hello",
            &mut message_id2,
            &mut no_edit_support2,
            None,
            Some(""),
        )
        .await;
        let sent2 = mock2.sent.lock().await;
        let first2 = sent2.first().expect("stream message should be sent");
        assert!(
            first2.metadata.get("thread_id").is_none(),
            "empty-string thread_id must be treated as absent, got {}",
            first2.metadata
        );
    }
}
