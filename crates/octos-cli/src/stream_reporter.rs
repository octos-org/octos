//! Progressive streaming reporter for messaging channels.
//!
//! Bridges the synchronous `ProgressReporter` trait to async channel I/O,
//! enabling real-time LLM text streaming to Telegram, WhatsApp, etc.
//! Text is accumulated and the channel message is edited at a throttled rate.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use octos_agent::progress::{ProgressEvent, ProgressReporter};
use octos_bus::{ActiveSessionStore, Channel};
use octos_core::{METADATA_SENDER_USER_ID, OutboundMessage, SessionKey};
use tokio::sync::{OwnedSemaphorePermit, RwLock, Semaphore, mpsc};
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

const STREAM_EVENT_CAPACITY: usize = 64;
const STREAM_EVENT_BYTE_BUDGET: usize = 1024 * 1024;
const CHUNK_FLUSH_BYTES: usize = 16 * 1024;

#[derive(Debug)]
struct StreamBudget {
    max_bytes: usize,
    semaphore: Arc<Semaphore>,
    queued_bytes: AtomicUsize,
    high_water_bytes: AtomicUsize,
    high_water_events: AtomicUsize,
    dropped_noncritical: AtomicU64,
}

impl StreamBudget {
    fn new(bytes: usize) -> Arc<Self> {
        Arc::new(Self {
            max_bytes: bytes,
            semaphore: Arc::new(Semaphore::new(bytes)),
            queued_bytes: AtomicUsize::new(0),
            high_water_bytes: AtomicUsize::new(0),
            high_water_events: AtomicUsize::new(0),
            dropped_noncritical: AtomicU64::new(0),
        })
    }

    fn track(&self, bytes: usize) {
        let current = self.queued_bytes.fetch_add(bytes, Ordering::AcqRel) + bytes;
        self.high_water_bytes.fetch_max(current, Ordering::AcqRel);
    }
}

/// A queued event owns its byte permits until it is processed OR the receiver
/// is dropped. Dropping a blocked send therefore also releases its budget.
pub struct StreamEventEnvelope {
    event: Option<StreamProgressEvent>,
    budget: Arc<StreamBudget>,
    bytes: usize,
    _permit: OwnedSemaphorePermit,
}

impl StreamEventEnvelope {
    fn new(
        event: StreamProgressEvent,
        budget: Arc<StreamBudget>,
        permit: OwnedSemaphorePermit,
    ) -> Self {
        let bytes = stream_event_bytes(&event);
        budget.track(bytes);
        Self {
            event: Some(event),
            budget,
            bytes,
            _permit: permit,
        }
    }
}

impl Drop for StreamEventEnvelope {
    fn drop(&mut self) {
        self.budget
            .queued_bytes
            .fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

#[derive(Clone)]
pub struct StreamEventSender {
    tx: mpsc::Sender<StreamEventEnvelope>,
    budget: Arc<StreamBudget>,
    /// Turn-level cancellation flag shared with the SessionActor's `cancelled`.
    /// When set (by the actor on cancel/interrupt/shutdown), `send_reliable`
    /// stops waiting and returns immediately, letting the agent's loop observe
    /// the flag on its next iteration and terminate instead of producing a
    /// fake-successful turn after its stream consumer died.
    cancelled: Option<Arc<AtomicBool>>,
}

pub struct StreamEventReceiver {
    inner: mpsc::Receiver<StreamEventEnvelope>,
    budget: Arc<StreamBudget>,
}

impl StreamEventReceiver {
    pub async fn recv(&mut self) -> Option<StreamEventEnvelope> {
        self.inner.recv().await
    }

    pub fn try_recv(&mut self) -> Result<StreamEventEnvelope, mpsc::error::TryRecvError> {
        self.inner.try_recv()
    }

    /// Close the receiver explicitly (e.g. when the turn is cancelled or the
    /// forwarder is aborted). Equivalent to dropping the receiver: queued
    /// envelopes are drained (releasing their byte permits) and both the
    /// channel and the byte-permit semaphore are closed so blocked producers
    /// wake immediately.
    pub fn close(&mut self) {
        self.inner.close();
        // Drain so every queued envelope's Drop releases its byte permits;
        // a producer blocked on the semaphore is woken by close below and
        // observes freed capacity or the closed channel on the next poll.
        while let Ok(envelope) = self.inner.try_recv() {
            drop(envelope);
        }
        self.budget.semaphore.close();
    }
}

impl Drop for StreamEventReceiver {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Copy)]
pub struct StreamEventStats {
    pub queued_bytes: usize,
    pub high_water_bytes: usize,
    pub high_water_events: usize,
    pub dropped_noncritical: u64,
    pub queued_events: usize,
}

pub fn stream_event_channel() -> (StreamEventSender, StreamEventReceiver) {
    stream_event_channel_with_limits(STREAM_EVENT_CAPACITY, STREAM_EVENT_BYTE_BUDGET)
}

pub fn stream_event_channel_with_limits(
    capacity: usize,
    byte_budget: usize,
) -> (StreamEventSender, StreamEventReceiver) {
    assert!(capacity > 0 && byte_budget > 0);
    let (tx, inner) = mpsc::channel(capacity);
    let budget = StreamBudget::new(byte_budget);
    (
        StreamEventSender {
            tx,
            budget: budget.clone(),
            cancelled: None,
        },
        StreamEventReceiver { inner, budget },
    )
}

fn stream_event_bytes(event: &StreamProgressEvent) -> usize {
    const EVENT_OVERHEAD: usize = 64;
    EVENT_OVERHEAD
        + match event {
            StreamProgressEvent::Chunk { text, .. } => text.len(),
            StreamProgressEvent::ToolStarted { name }
            | StreamProgressEvent::ToolCompleted { name, .. }
            | StreamProgressEvent::FileWritten { path: name } => name.len(),
            StreamProgressEvent::ToolProgress { name, message } => name.len() + message.len(),
            StreamProgressEvent::LlmStatus { message } => message.len(),
            StreamProgressEvent::RawSse { json } => json.len(),
            StreamProgressEvent::StreamDone { .. } | StreamProgressEvent::BufferReset => 0,
        }
}

impl StreamEventSender {
    /// Bind the turn's cancellation flag so a reliable send stops waiting the
    /// moment the actor cancels/interrupts the turn or its consumer is dead.
    pub fn with_cancel_flag(mut self, cancelled: Arc<AtomicBool>) -> Self {
        self.cancelled = Some(cancelled);
        self
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled
            .as_ref()
            .is_some_and(|c| c.load(Ordering::Acquire))
    }

    fn try_reserve(&self, bytes: usize) -> Option<OwnedSemaphorePermit> {
        let bytes = u32::try_from(bytes).unwrap_or(u32::MAX);
        self.budget
            .semaphore
            .clone()
            .try_acquire_many_owned(bytes)
            .ok()
    }

    fn track_event_high_water(&self) {
        let queued = self.tx.max_capacity() - self.tx.capacity();
        self.budget
            .high_water_events
            .fetch_max(queued, Ordering::AcqRel);
    }

    /// Drop only explicitly noncritical progress when either bound is full.
    pub fn send_noncritical(&self, event: StreamProgressEvent) {
        let bytes = stream_event_bytes(&event);
        let Some(permit) = self.try_reserve(bytes) else {
            self.budget
                .dropped_noncritical
                .fetch_add(1, Ordering::Relaxed);
            metrics::counter!("octos_stream_progress_dropped_total", "reason" => "byte_budget")
                .increment(1);
            return;
        };
        let envelope = StreamEventEnvelope::new(event, self.budget.clone(), permit);
        match self.tx.try_send(envelope) {
            Ok(()) => self.track_event_high_water(),
            Err(_) => {
                self.budget
                    .dropped_noncritical
                    .fetch_add(1, Ordering::Relaxed);
                metrics::counter!("octos_stream_progress_dropped_total", "reason" => "queue_full")
                    .increment(1);
            }
        }
    }

    /// Preserve content, tool lifecycle and terminal events. On a multi-thread
    /// runtime `block_in_place` applies real backpressure without deadlocking
    /// the receiver. Current-thread hosts use a shared independent forwarder
    /// runtime, so producers can wait without stalling their receiver.
    ///
    /// Cancellation contract: a blocked reliable send aborts (returning
    /// without delivering) when the receiver is closed. Every SessionActor
    /// cancel/interrupt/shutdown path pairs its forwarder abort with a bound
    /// turn `cancelled` flag; on the current-thread/standalone branches the
    /// 1 ms poll loop additionally observes the flag and exits early, while on
    /// the multi-thread production path the release comes from the receiver
    /// close (the parked future is not re-polled on the flag alone). After
    /// either release the agent loop terminates on the flag at its next
    /// iteration/budget check, so the turn can never proceed to a
    /// fake-successful completion with its stream consumer dead.
    pub fn send_reliable(&self, event: StreamProgressEvent) {
        let sender = self.clone();
        let send = async move {
            if sender.is_cancelled() {
                return;
            }
            let bytes = stream_event_bytes(&event);
            // A single reliable event larger than the budget is delivered
            // alone rather than deadlocked or silently dropped. Its actual
            // payload remains visible in queued/high-water accounting.
            let reserved = bytes.min(sender.budget.max_bytes);
            if reserved < bytes {
                metrics::counter!("octos_stream_oversize_reliable_total").increment(1);
            }
            let reserved = u32::try_from(reserved).unwrap_or(u32::MAX);
            let permit = match sender
                .budget
                .semaphore
                .clone()
                .acquire_many_owned(reserved)
                .await
            {
                Ok(permit) => permit,
                Err(_) => return,
            };
            if sender.is_cancelled() {
                return;
            }
            let envelope = StreamEventEnvelope::new(event, sender.budget.clone(), permit);
            if sender.tx.send(envelope).await.is_ok() {
                sender.track_event_high_water();
            }
        };
        let cancelled = self.cancelled.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(|| handle.block_on(send));
            }
            // `send_reliable` may run on a current-thread runtime worker whose
            // forwarder is hosted by the process-wide fallback runtime. The
            // fallback's mpsc/semaphore wakers can wake this future while the
            // worker is blocked, so a bounded send cannot deadlock its own
            // consumer.
            Ok(handle) => {
                let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let done_clone = std::sync::Arc::clone(&done);
                std::thread::spawn(move || {
                    handle.block_on(async move {
                        send.await;
                        done_clone.store(true, std::sync::atomic::Ordering::Release);
                    });
                });
                // The helper thread may run before the caller registers its
                // waiter, so poll with a short sleep instead of blocking on
                // the helper's waker directly. Exit when the helper finishes
                // OR the turn is cancelled — whichever comes first.
                while !done.load(std::sync::atomic::Ordering::Acquire) {
                    if cancelled
                        .as_ref()
                        .is_some_and(|c| c.load(Ordering::Acquire))
                    {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            Err(_) => {
                let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let done_clone = std::sync::Arc::clone(&done);
                std::thread::spawn(move || {
                    tokio::runtime::Builder::new_current_thread()
                        .build()
                        .expect("stream send runtime")
                        .block_on(async move {
                            send.await;
                            done_clone.store(true, std::sync::atomic::Ordering::Release);
                        });
                });
                // Same poll-based wait for a standalone thread. Exit when the
                // helper finishes or the turn is cancelled.
                while !done.load(std::sync::atomic::Ordering::Acquire) {
                    if cancelled
                        .as_ref()
                        .is_some_and(|c| c.load(Ordering::Acquire))
                    {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
    }

    #[cfg(test)]
    pub fn stats(&self) -> StreamEventStats {
        StreamEventStats {
            queued_bytes: self.budget.queued_bytes.load(Ordering::Acquire),
            high_water_bytes: self.budget.high_water_bytes.load(Ordering::Acquire),
            high_water_events: self.budget.high_water_events.load(Ordering::Acquire),
            dropped_noncritical: self.budget.dropped_noncritical.load(Ordering::Relaxed),
            queued_events: self.tx.max_capacity() - self.tx.capacity(),
        }
    }
}

/// A `ProgressReporter` that forwards stream events through a bounded channel.
///
/// `ProgressReporter::report()` is synchronous, so reliable events use bounded
/// synchronous backpressure and text deltas are coalesced before enqueueing.
///
/// M8.10 PR #2: every emitted SSE payload includes `thread_id` (the
/// client_message_id of the user message that owns this turn). Reporters are
/// constructed per-turn so the thread_id is bound for the reporter's lifetime.
/// When `thread_id` is `None`, the field is omitted (preserves wire compat for
/// non-API channels and pre-cmid clients).
pub struct ChannelStreamReporter {
    tx: StreamEventSender,
    thread_id: Option<String>,
    pending: std::sync::Mutex<PendingChunk>,
}

#[derive(Default)]
struct PendingChunk {
    text: String,
    iteration: u32,
}

impl PendingChunk {
    fn take(&mut self) -> Option<StreamProgressEvent> {
        if self.text.is_empty() {
            return None;
        }
        Some(StreamProgressEvent::Chunk {
            text: std::mem::take(&mut self.text),
            iteration: self.iteration,
        })
    }
}

impl ChannelStreamReporter {
    pub fn new(tx: StreamEventSender) -> Self {
        Self {
            tx,
            thread_id: None,
            pending: std::sync::Mutex::new(PendingChunk::default()),
        }
    }

    fn flush_pending(&self) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(event) = pending.take() {
            self.tx.send_reliable(event);
        }
    }

    fn push_chunk(&self, text: String, iteration: u32) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if !pending.text.is_empty() && pending.iteration != iteration
            || pending.text.len() >= CHUNK_FLUSH_BYTES
        {
            if let Some(event) = pending.take() {
                self.tx.send_reliable(event);
            }
        }
        pending.iteration = iteration;
        pending.text.push_str(&text);
        if pending.text.len() >= CHUNK_FLUSH_BYTES
            && let Some(event) = pending.take()
        {
            self.tx.send_reliable(event);
        }
    }

    /// Bind a `thread_id` (typically the user message's `client_message_id`)
    /// to every SSE payload this reporter emits.
    pub fn with_thread_id(mut self, thread_id: Option<String>) -> Self {
        self.thread_id = thread_id.filter(|s| !s.is_empty());
        self
    }
}

impl Drop for ChannelStreamReporter {
    fn drop(&mut self) {
        self.flush_pending();
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
        if let ProgressEvent::StreamChunk { text, iteration } = event {
            self.push_chunk(text, iteration);
            return;
        }
        self.flush_pending();
        let thread_id = self.thread_id.as_deref();
        let mut reliable = true;
        let mapped = match event {
            ProgressEvent::ReasoningChunk { text, iteration } => {
                let mut payload = serde_json::json!({
                    "type": "reasoning_chunk",
                    "text": text,
                    "iteration": iteration,
                });
                inject_thread_id(&mut payload, thread_id);
                StreamProgressEvent::RawSse {
                    json: payload.to_string(),
                }
            }
            ProgressEvent::StreamDone { iteration } => {
                StreamProgressEvent::StreamDone { iteration }
            }
            ProgressEvent::ToolStarted {
                ref name,
                ref tool_id,
                ..
            } => {
                // Also send raw SSE for web client status indicators
                let mut payload = serde_json::json!({
                    "type": "tool_start",
                    "tool": name,
                    "tool_call_id": tool_id,
                });
                inject_thread_id(&mut payload, thread_id);
                self.tx.send_reliable(StreamProgressEvent::RawSse {
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
                self.tx.send_reliable(StreamProgressEvent::RawSse {
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
                reliable = false;
                let mut payload = serde_json::json!({
                    "type": "tool_progress",
                    "tool": name,
                    "tool_call_id": tool_id,
                    "message": message,
                });
                inject_thread_id(&mut payload, thread_id);
                self.tx.send_reliable(StreamProgressEvent::RawSse {
                    json: payload.to_string(),
                });
                StreamProgressEvent::ToolProgress {
                    name: name.clone(),
                    message: message.clone(),
                }
            }
            ProgressEvent::LlmStatus { message, .. } => {
                reliable = false;
                StreamProgressEvent::LlmStatus { message }
            }
            ProgressEvent::FileModified { path, .. } => StreamProgressEvent::FileWritten { path },
            ProgressEvent::StreamRetry { .. } => StreamProgressEvent::BufferReset,
            // Forward discrete progress events as raw SSE JSON for the web client.
            ProgressEvent::Thinking { iteration } => {
                let mut payload = serde_json::json!({"type": "thinking", "iteration": iteration});
                inject_thread_id(&mut payload, thread_id);
                StreamProgressEvent::RawSse {
                    json: payload.to_string(),
                }
            }
            ProgressEvent::AgentProgress {
                iteration,
                active_tokens,
                elapsed,
                checkpoints,
                reflecting,
            } => {
                let mut payload = serde_json::json!({
                    "type": "agent_progress",
                    "message": octos_agent::progress::agent_progress_message(
                        iteration,
                        active_tokens,
                        elapsed,
                        checkpoints,
                        reflecting,
                    ),
                    "iteration": iteration,
                    "active_tokens": active_tokens,
                    "elapsed_ms": elapsed.as_millis() as u64,
                    "checkpoints": checkpoints,
                    "reflecting": reflecting,
                });
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
                model,
                context_window,
                ..
            } => {
                // Codex round-1 P2: every wire-shape mapper for
                // `cost_update` must carry `model` so the chat bubble
                // footer (`model · tokens_in / tokens_out · duration`)
                // works regardless of which reporter the turn uses.
                // Without this, channel-stream consumers see naked
                // token counts even after the agent emit layer
                // attached a model id.
                let mut payload = serde_json::json!({
                    "type": "cost_update",
                    "input_tokens": session_input_tokens,
                    "output_tokens": session_output_tokens,
                    "session_cost": session_cost,
                });
                if let Some(model) = model.as_deref() {
                    payload["model"] = serde_json::Value::String(model.to_string());
                }
                // Carry the context window too so channel/web clients on this
                // path render an honest ctx gauge, matching the BoundedChannel
                // (`event_to_json`) path.
                if let Some(window) = context_window {
                    payload["context_window"] = serde_json::json!(window);
                }
                inject_thread_id(&mut payload, thread_id);
                StreamProgressEvent::RawSse {
                    json: payload.to_string(),
                }
            }
            _ => return,
        };
        if reliable {
            self.tx.send_reliable(mapped);
        } else {
            self.tx.send_noncritical(mapped);
        }
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
    /// Set when an `app_reply`-tagged tool (e.g. `send_app_card`) succeeded and
    /// the forwarder suppressed the LLM's follow-up text: the GPU card IS the
    /// reply on capable clients. The session actor must then NOT also send
    /// `conv_response.content` through the final-reply path, or the user sees
    /// the card plus a duplicate text bubble (review finding #6).
    pub suppressed_by_app_reply: bool,
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

/// Spawn the consumer independently of a current-thread producer. Multi-thread
/// production keeps the original async task; a shared fallback runtime covers
/// current-thread hosts without per-session runtimes or blocking-pool tasks.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub fn spawn_stream_forwarder(
    rx: StreamEventReceiver,
    channel: Arc<dyn Channel>,
    chat_id: String,
    cancel_status: Option<Arc<std::sync::atomic::AtomicBool>>,
    status_msg_id: Option<Arc<tokio::sync::Mutex<Option<String>>>>,
    active_sessions: Arc<RwLock<ActiveSessionStore>>,
    session_key: SessionKey,
    sender_user_id: Option<String>,
    operation_updater: Option<Arc<dyn Fn(&str) + Send + Sync>>,
    thread_id: Option<String>,
    matrix_app_reply_tools: Arc<HashSet<String>>,
) -> tokio::task::JoinHandle<StreamResult> {
    let forward = run_stream_forwarder(
        rx,
        channel,
        chat_id,
        cancel_status,
        status_msg_id,
        active_sessions,
        session_key,
        sender_user_id,
        operation_updater,
        thread_id,
        matrix_app_reply_tools,
    );
    if tokio::runtime::Handle::current().runtime_flavor()
        == tokio::runtime::RuntimeFlavor::MultiThread
    {
        tokio::spawn(forward)
    } else {
        // One process-wide fallback runtime drives I/O/timers for hosts using
        // a current-thread runtime. It is shared, not one thread per session.
        static FALLBACK_RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> =
            std::sync::OnceLock::new();
        FALLBACK_RUNTIME
            .get_or_init(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .thread_name("octos-stream-forwarder")
                    .build()
                    .expect("stream forwarder fallback runtime")
            })
            .spawn(forward)
    }
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
    mut rx: StreamEventReceiver,
    channel: Arc<dyn Channel>,
    chat_id: String,
    cancel_status: Option<Arc<std::sync::atomic::AtomicBool>>,
    status_msg_id: Option<Arc<tokio::sync::Mutex<Option<String>>>>,
    active_sessions: Arc<RwLock<ActiveSessionStore>>,
    session_key: SessionKey,
    sender_user_id: Option<String>,
    operation_updater: Option<Arc<dyn Fn(&str) + Send + Sync>>,
    thread_id: Option<String>,
    matrix_app_reply_tools: Arc<HashSet<String>>,
) -> StreamResult {
    let mut buffer = String::new();
    let mut message_id: Option<String> = None;
    let mut last_edit = Instant::now() - EDIT_THROTTLE; // allow immediate first edit
    let mut first_chunk = true;
    let mut last_chunk_iteration: u32 = 0;
    // Matrix app-reply hook: when an app_reply-tagged tool is registered and
    // we're on matrix, the GPU card IS the reply on capable clients —
    // suppress transient tool status edits, and once an app-reply tool
    // succeeds, drop the LLM's follow-up text so the user doesn't see a
    // duplicate chat-style summary under the card.
    let suppress_matrix_transient_status =
        channel.name() == "matrix" && !matrix_app_reply_tools.is_empty();
    let mut suppress_follow_up_text_after_app_reply = false;
    // When true, the channel doesn't support send_with_id (returned None),
    // so we stop streaming edits and let the final reply go through out_tx.
    let mut no_edit_support = false;

    while let Some(mut envelope) = rx.recv().await {
        let event = envelope
            .event
            .take()
            .expect("stream envelope contains an event");
        match event {
            StreamProgressEvent::Chunk { text, iteration } => {
                if suppress_follow_up_text_after_app_reply {
                    continue;
                }
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
                if suppress_follow_up_text_after_app_reply {
                    continue;
                }
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
                if suppress_matrix_transient_status {
                    continue;
                }
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
                if success && matrix_app_reply_tools.contains(&name) {
                    suppress_follow_up_text_after_app_reply = true;
                    buffer.clear();
                    continue;
                }
                if suppress_matrix_transient_status {
                    continue;
                }
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
                if suppress_follow_up_text_after_app_reply || suppress_matrix_transient_status {
                    continue;
                }
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
                if suppress_follow_up_text_after_app_reply || suppress_matrix_transient_status {
                    continue;
                }
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
                if suppress_follow_up_text_after_app_reply || suppress_matrix_transient_status {
                    continue;
                }
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
                if suppress_follow_up_text_after_app_reply {
                    continue;
                }
                // Clear accumulated text so a retry starts fresh.
                // Keep the message_id so the retry edits the same message
                // instead of creating a new one.
                buffer.clear();
            }
            StreamProgressEvent::RawSse { json } => {
                // Forward raw SSE JSON directly to the channel.
                // Only ApiChannel implements this; other channels ignore it.
                //
                // PR F (M8.10 thread-binding chain): use the `_bound`
                // variant so the originating turn's `thread_id`
                // (captured at this forwarder's task spawn) overrides
                // any stale `thread_id` field embedded in `json` and
                // short-circuits the api_channel's sticky-map fallback.
                let _ = channel
                    .send_raw_sse_bound(&chat_id, &json, thread_id.as_deref())
                    .await;
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
        suppressed_by_app_reply: suppress_follow_up_text_after_app_reply,
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
        // PR F (M8.10 thread-binding chain `#649 → #740`): use the
        // `_bound` variants so the originating turn's `thread_id`
        // (captured at this forwarder's task spawn and threaded through
        // `flush_to_channel`/`finish_flush_to_channel`) is the ONLY
        // input to the api_channel's wire-side `thread_id` resolution
        // when the bound id is present. Falling back to the legacy
        // (non-bound) trait methods here re-opens LEAK 2 — the sticky
        // map rotates under rapid-fire so late deltas of an earlier
        // turn would mis-route to a later turn's bubble.
        let result = if finish {
            channel
                .finish_stream_bound(chat_id, mid, text, thread_id)
                .await
        } else {
            channel
                .edit_message_bound(chat_id, mid, text, thread_id)
                .await
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
