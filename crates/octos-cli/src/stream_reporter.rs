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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use async_trait::async_trait;
    use octos_core::{InboundMessage, METADATA_SENDER_USER_ID};
    use tokio::sync::{Mutex, mpsc};

    /// PR F (M8.10): record every `_bound` call's `thread_id`
    /// argument so the test can assert the forwarder threads it
    /// through the trait correctly.
    type BoundCall = (&'static str, Option<String>);

    fn try_recv_event(
        rx: &mut StreamEventReceiver,
    ) -> Result<StreamProgressEvent, mpsc::error::TryRecvError> {
        rx.try_recv()
            .map(|mut envelope| envelope.event.take().expect("event envelope"))
    }

    #[derive(Default)]
    struct MockChannel {
        sent: Arc<Mutex<Vec<OutboundMessage>>>,
        bound_thread_ids: Arc<Mutex<Vec<BoundCall>>>,
        send_delay: Duration,
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
            if !self.send_delay.is_zero() {
                tokio::time::sleep(self.send_delay).await;
            }
            self.sent.lock().await.push(msg.clone());
            Ok(Some("$stream-1".to_string()))
        }

        fn supports_edit(&self) -> bool {
            true
        }

        async fn edit_message_bound(
            &self,
            _chat_id: &str,
            _message_id: &str,
            _new_content: &str,
            thread_id: Option<&str>,
        ) -> eyre::Result<()> {
            self.bound_thread_ids
                .lock()
                .await
                .push(("edit_message_bound", thread_id.map(str::to_string)));
            Ok(())
        }

        async fn finish_stream_bound(
            &self,
            _chat_id: &str,
            _message_id: &str,
            _final_content: &str,
            thread_id: Option<&str>,
        ) -> eyre::Result<()> {
            self.bound_thread_ids
                .lock()
                .await
                .push(("finish_stream_bound", thread_id.map(str::to_string)));
            Ok(())
        }

        async fn send_raw_sse_bound(
            &self,
            _chat_id: &str,
            _json: &str,
            thread_id: Option<&str>,
        ) -> eyre::Result<()> {
            self.bound_thread_ids
                .lock()
                .await
                .push(("send_raw_sse_bound", thread_id.map(str::to_string)));
            Ok(())
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

        let (tx, mut rx) = stream_event_channel();
        let reporter = ChannelStreamReporter::new(tx);

        reporter.report(ProgressEvent::StreamRetry { iteration: 1 });

        let event = try_recv_event(&mut rx).unwrap();
        assert!(matches!(event, StreamProgressEvent::BufferReset));
    }

    /// M8.10 PR #2: every raw SSE payload emitted by the reporter must
    /// include a `thread_id` field equal to the bound cmid. Drives every
    /// variant that produces a `RawSse` event to confirm none are missed.
    #[test]
    fn should_inject_thread_id_into_every_raw_sse_event() {
        use octos_agent::progress::ProgressEvent;
        use std::time::Duration;

        let (tx, mut rx) = stream_event_channel();
        let reporter =
            ChannelStreamReporter::new(tx).with_thread_id(Some("cmid-thread-A".to_string()));

        reporter.report(ProgressEvent::ToolStarted {
            name: "shell".into(),
            tool_id: "t1".into(),
            arguments: None,
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
        reporter.report(ProgressEvent::ReasoningChunk {
            text: "thinking".into(),
            iteration: 1,
        });
        reporter.report(ProgressEvent::CostUpdate {
            session_input_tokens: 10,
            session_output_tokens: 20,
            turn_input_tokens: 10,
            turn_output_tokens: 20,
            response_cost: None,
            session_cost: None,
            model: None,
            context_window: None,
        });

        let mut raw_payloads: Vec<String> = Vec::new();
        while let Ok(event) = try_recv_event(&mut rx) {
            if let StreamProgressEvent::RawSse { json } = event {
                raw_payloads.push(json);
            }
        }

        // ToolStarted, ToolCompleted, ToolProgress emit RawSse + a typed
        // mapped event each, so 7 reports → 7 RawSse JSON payloads.
        assert_eq!(
            raw_payloads.len(),
            7,
            "expected 7 RawSse payloads, got {}: {:?}",
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

    /// The channel stream reporter feeds API/channel clients that read
    /// the wire payload directly. Codex round-1 P2 — when the agent
    /// emit layer attaches a `model` id to `CostUpdate`, this reporter
    /// must thread it through so the chat bubble footer (`model ·
    /// tokens_in / tokens_out · duration`) works for channel
    /// consumers, not just the UI-protocol bridge.
    #[test]
    fn cost_update_carries_model_into_raw_sse_payload() {
        use octos_agent::progress::ProgressEvent;

        let (tx, mut rx) = stream_event_channel();
        let reporter = ChannelStreamReporter::new(tx);

        reporter.report(ProgressEvent::CostUpdate {
            session_input_tokens: 12,
            session_output_tokens: 7,
            turn_input_tokens: 12,
            turn_output_tokens: 7,
            response_cost: None,
            session_cost: None,
            model: Some("deepseek-v4-pro".into()),
            context_window: None,
        });

        let StreamProgressEvent::RawSse { json } = try_recv_event(&mut rx).unwrap() else {
            panic!("expected RawSse for CostUpdate");
        };
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "cost_update");
        assert_eq!(parsed["model"], "deepseek-v4-pro");
    }

    /// The channel-stream path must also carry the model context window so
    /// channel/web clients render an honest ctx gauge, matching the
    /// BoundedChannel (`event_to_json`) path.
    #[test]
    fn cost_update_carries_context_window_into_raw_sse_payload() {
        use octos_agent::progress::ProgressEvent;

        let (tx, mut rx) = stream_event_channel();
        let reporter = ChannelStreamReporter::new(tx);

        reporter.report(ProgressEvent::CostUpdate {
            session_input_tokens: 12,
            session_output_tokens: 7,
            turn_input_tokens: 12,
            turn_output_tokens: 7,
            response_cost: None,
            session_cost: None,
            model: None,
            context_window: Some(131_072),
        });

        let StreamProgressEvent::RawSse { json } = try_recv_event(&mut rx).unwrap() else {
            panic!("expected RawSse for CostUpdate");
        };
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "cost_update");
        assert_eq!(parsed["context_window"], 131_072);
    }

    /// Symmetric to the test above: a `CostUpdate` without `model`
    /// must not synthesise a `model` field on the wire — the field is
    /// strictly opt-in so legacy parsers that key on field presence
    /// don't trip.
    #[test]
    fn cost_update_omits_model_when_absent() {
        use octos_agent::progress::ProgressEvent;

        let (tx, mut rx) = stream_event_channel();
        let reporter = ChannelStreamReporter::new(tx);

        reporter.report(ProgressEvent::CostUpdate {
            session_input_tokens: 12,
            session_output_tokens: 7,
            turn_input_tokens: 12,
            turn_output_tokens: 7,
            response_cost: None,
            session_cost: None,
            model: None,
            context_window: None,
        });

        let StreamProgressEvent::RawSse { json } = try_recv_event(&mut rx).unwrap() else {
            panic!("expected RawSse for CostUpdate");
        };
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(
            parsed.get("model").is_none(),
            "model field must be omitted when the emit layer didn't set one, got {parsed}",
        );
    }

    /// When the reporter is constructed without a thread_id (or with an
    /// empty string), payloads must NOT carry a `thread_id` field. This
    /// preserves wire compatibility with non-API channels and pre-cmid
    /// clients that expect the field to be absent.
    #[test]
    fn should_omit_thread_id_when_not_bound() {
        use octos_agent::progress::ProgressEvent;

        let (tx, mut rx) = stream_event_channel();
        let reporter = ChannelStreamReporter::new(tx);

        reporter.report(ProgressEvent::Thinking { iteration: 0 });

        if let StreamProgressEvent::RawSse { json } = try_recv_event(&mut rx).unwrap() {
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

    /// Matrix app-reply hook: once an app_reply-tagged tool succeeds, the
    /// GPU card IS the reply — the forwarder must drop the transient tool
    /// status edits and the LLM's follow-up text so the user doesn't see a
    /// duplicate chat-style summary under the card.
    #[tokio::test]
    async fn should_skip_transcript_and_follow_up_chunks_when_matrix_app_reply_succeeds() {
        let dir = tempfile::TempDir::new().unwrap();
        let active_sessions = Arc::new(RwLock::new(ActiveSessionStore::open(dir.path()).unwrap()));
        let mock = Arc::new(MockChannel::default());
        let channel: Arc<dyn Channel> = mock.clone();
        let (tx, rx) = stream_event_channel();

        tx.send_reliable(StreamProgressEvent::ToolStarted {
            name: "send_app_card".to_string(),
        });
        tx.send_reliable(StreamProgressEvent::ToolCompleted {
            name: "send_app_card".to_string(),
            success: true,
        });
        tx.send_reliable(StreamProgressEvent::Chunk {
            text: "hallucinated weather summary".to_string(),
            iteration: 1,
        });
        tx.send_reliable(StreamProgressEvent::StreamDone { iteration: 1 });
        drop(tx);

        let result = run_stream_forwarder(
            rx,
            channel,
            "!room:localhost".to_string(),
            None,
            None,
            active_sessions,
            octos_core::SessionKey::new("matrix", "!room:localhost"),
            Some("@octosbot:localhost".to_string()),
            None,
            None,
            Arc::new(HashSet::from(["send_app_card".to_string()])),
        )
        .await;

        assert!(result.message_id.is_none());
        assert!(result.text.is_empty());
        assert!(mock.sent.lock().await.is_empty());
        // Review finding #6: the suppression decision must be propagated so the
        // session actor's final-reply path can skip conv_response.content.
        assert!(
            result.suppressed_by_app_reply,
            "app-reply suppression must be reported to the caller"
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

    /// PR F (M8.10 thread-binding chain `#649 → #740`): when
    /// `do_flush` (via `flush_to_channel`/`finish_flush_to_channel`)
    /// receives a non-`None` `thread_id`, it must thread that through
    /// the `_bound` Channel trait methods rather than the legacy
    /// `edit_message`/`finish_stream` (which fall back to the
    /// api_channel's per-chat sticky map). The mock asserts the actual
    /// argument observed.
    #[tokio::test]
    async fn do_flush_passes_thread_id_to_bound_methods() {
        let mock = Arc::new(MockChannel::default());
        let bound_calls = Arc::clone(&mock.bound_thread_ids);
        let channel: Arc<dyn Channel> = mock.clone();

        // edit path: message_id is set so do_flush dispatches to
        // `edit_message_bound`. The forwarder's captured `thread_id` is
        // forwarded as Some("cmid-A").
        let mut message_id = Some("$stream-1".to_string());
        let mut no_edit_support = false;
        flush_to_channel(
            &channel,
            "chat-A",
            "hello",
            &mut message_id,
            &mut no_edit_support,
            None,
            Some("cmid-A"),
        )
        .await;
        // finish path: dispatches to `finish_stream_bound`.
        finish_flush_to_channel(
            &channel,
            "chat-A",
            "hello (final)",
            &mut message_id,
            &mut no_edit_support,
            None,
            Some("cmid-A"),
        )
        .await;

        let calls = bound_calls.lock().await.clone();
        assert!(
            calls.iter().any(
                |(name, tid)| *name == "edit_message_bound" && tid.as_deref() == Some("cmid-A")
            ),
            "expected edit_message_bound call with thread_id=cmid-A, got: {calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|(name, tid)| *name == "finish_stream_bound"
                    && tid.as_deref() == Some("cmid-A")),
            "expected finish_stream_bound call with thread_id=cmid-A, got: {calls:?}"
        );
    }

    #[test]
    fn should_coalesce_utf8_chunks_before_tool_and_terminal_events() {
        use octos_agent::progress::ProgressEvent;

        let (tx, mut rx) = stream_event_channel_with_limits(4, 4096);
        let reporter = ChannelStreamReporter::new(tx);
        for text in ["你", "好", "🙂", " world"] {
            reporter.report(ProgressEvent::StreamChunk {
                text: text.to_owned(),
                iteration: 7,
            });
        }
        reporter.report(ProgressEvent::StreamDone { iteration: 7 });
        let first = try_recv_event(&mut rx).unwrap();
        let StreamProgressEvent::Chunk { text, iteration } = first else {
            panic!("expected coalesced chunk");
        };
        assert_eq!(text, "你好🙂 world");
        assert_eq!(iteration, 7);
        assert!(matches!(
            try_recv_event(&mut rx).unwrap(),
            StreamProgressEvent::StreamDone { iteration: 7 }
        ));
    }

    #[test]
    fn should_backpressure_without_dropping_or_reordering_critical_events() {
        let (tx, mut rx) = stream_event_channel_with_limits(1, 512);
        tx.send_reliable(StreamProgressEvent::ToolStarted {
            name: "shell".to_owned(),
        });
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let producer = std::thread::spawn(move || {
            tx.send_reliable(StreamProgressEvent::ToolCompleted {
                name: "shell".to_owned(),
                success: true,
            });
            tx.send_reliable(StreamProgressEvent::StreamDone { iteration: 1 });
            done_tx.send(()).unwrap();
        });
        assert!(done_rx.recv_timeout(Duration::from_millis(20)).is_err());
        let mut names = Vec::new();
        for _ in 0..3 {
            let event = loop {
                if let Ok(event) = try_recv_event(&mut rx) {
                    break event;
                }
                std::thread::sleep(Duration::from_millis(2));
            };
            names.push(match event {
                StreamProgressEvent::ToolStarted { .. } => "start",
                StreamProgressEvent::ToolCompleted { .. } => "complete",
                StreamProgressEvent::StreamDone { .. } => "done",
                _ => panic!("unexpected event"),
            });
        }
        done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        producer.join().unwrap();
        assert_eq!(names, ["start", "complete", "done"]);
    }

    #[test]
    fn should_unblock_reliable_producer_when_receiver_is_dropped() {
        let (tx, rx) = stream_event_channel_with_limits(1, 512);
        tx.send_reliable(StreamProgressEvent::ToolStarted {
            name: "first".to_owned(),
        });
        let stats_tx = tx.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let producer = std::thread::spawn(move || {
            tx.send_reliable(StreamProgressEvent::StreamDone { iteration: 1 });
            done_tx.send(()).unwrap();
        });
        std::thread::sleep(Duration::from_millis(20));
        drop(rx);
        done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        producer.join().unwrap();
        assert_eq!(stats_tx.stats().queued_bytes, 0);
    }

    #[test]
    fn should_unblock_byte_budget_waiter_when_receiver_is_dropped() {
        let (tx, rx) = stream_event_channel_with_limits(4, 128);
        tx.send_reliable(StreamProgressEvent::ToolStarted {
            name: "x".repeat(64),
        });
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let producer = std::thread::spawn(move || {
            tx.send_reliable(StreamProgressEvent::StreamDone { iteration: 1 });
            done_tx.send(()).unwrap();
        });
        std::thread::sleep(Duration::from_millis(20));
        drop(rx);
        done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        producer.join().unwrap();
    }

    #[test]
    fn should_deliver_single_oversize_reliable_event_without_deadlock() {
        let (tx, mut rx) = stream_event_channel_with_limits(1, 64);
        tx.send_reliable(StreamProgressEvent::ToolStarted {
            name: "x".repeat(128),
        });
        assert_eq!(tx.stats().high_water_bytes, 192);
        match try_recv_event(&mut rx).unwrap() {
            StreamProgressEvent::ToolStarted { name } => assert_eq!(name.len(), 128),
            _ => panic!("expected reliable oversize event"),
        }
        assert_eq!(tx.stats().queued_bytes, 0);
    }

    #[test]
    fn should_enforce_byte_budget_with_dequeued_envelope_still_inflight() {
        let (tx, mut rx) = stream_event_channel_with_limits(4, 128);
        tx.send_reliable(StreamProgressEvent::ToolStarted {
            name: "x".repeat(64),
        });
        let held = rx.try_recv().unwrap();
        assert_eq!(tx.stats().queued_bytes, 128);
        tx.send_noncritical(StreamProgressEvent::LlmStatus {
            message: "drop by bytes".to_owned(),
        });
        assert_eq!(tx.stats().dropped_noncritical, 1);
        drop(held);
        assert_eq!(tx.stats().queued_bytes, 0);
    }

    #[test]
    fn should_flush_pending_chunk_on_reporter_drop_after_receiver_close() {
        use octos_agent::progress::ProgressEvent;

        let (tx, rx) = stream_event_channel_with_limits(1, 128);
        let reporter = ChannelStreamReporter::new(tx);
        reporter.report(ProgressEvent::StreamChunk {
            text: "pending".to_owned(),
            iteration: 1,
        });
        drop(rx);
        drop(reporter);
    }

    #[test]
    fn should_count_only_noncritical_progress_drops() {
        let (tx, mut rx) = stream_event_channel_with_limits(1, 512);
        tx.send_reliable(StreamProgressEvent::ToolStarted {
            name: "critical".to_owned(),
        });
        tx.send_noncritical(StreamProgressEvent::LlmStatus {
            message: "replaceable".to_owned(),
        });
        assert_eq!(tx.stats().dropped_noncritical, 1);
        assert!(matches!(
            try_recv_event(&mut rx).unwrap(),
            StreamProgressEvent::ToolStarted { .. }
        ));
        assert!(try_recv_event(&mut rx).is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn should_release_blocked_producer_when_forwarder_is_aborted() {
        let dir = tempfile::TempDir::new().unwrap();
        let active_sessions = Arc::new(RwLock::new(ActiveSessionStore::open(dir.path()).unwrap()));
        let mock = Arc::new(MockChannel {
            send_delay: Duration::from_millis(100),
            ..Default::default()
        });
        let (tx, rx) = stream_event_channel_with_limits(1, 64 * 1024);
        let forwarder = spawn_stream_forwarder(
            rx,
            mock,
            "!room:localhost".to_owned(),
            None,
            None,
            active_sessions,
            SessionKey::new("matrix", "!room:localhost"),
            None,
            None,
            None,
            Arc::new(HashSet::new()),
        );
        tx.send_reliable(StreamProgressEvent::Chunk {
            text: "x".repeat(CHUNK_FLUSH_BYTES),
            iteration: 1,
        });
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let producer = std::thread::spawn(move || {
            tx.send_reliable(StreamProgressEvent::StreamDone { iteration: 1 });
            done_tx.send(()).unwrap();
        });
        std::thread::sleep(Duration::from_millis(20));
        forwarder.abort();
        let _ = forwarder.await;
        done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        producer.join().unwrap();
    }

    /// P1 regression (receiver-close cancellation): a reliable producer parked
    /// on a FULL queue must be released when the bound turn flag flips — the
    /// path the SessionActor takes on cancel/interrupt/inbox-close when
    /// `abort()` cannot reach code parked inside `block_in_place`. The send
    /// must NOT complete silently: it returns without delivering, so the
    /// agent loop's next shutdown/budget check terminates the turn instead of
    /// continuing to LLM/tool calls and reporting a fake success.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn should_release_blocked_producer_when_turn_cancel_flag_is_set() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let (tx, rx) = stream_event_channel_with_limits(1, 64 * 1024);
        let tx = tx.with_cancel_flag(Arc::clone(&cancelled));
        // Fill the single slot so the next reliable send blocks.
        tx.send_reliable(StreamProgressEvent::ToolStarted {
            name: "held".to_owned(),
        });
        let stats_tx = tx.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let producer = std::thread::spawn(move || {
            tx.send_reliable(StreamProgressEvent::StreamDone { iteration: 1 });
            done_tx.send(()).unwrap();
        });
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            done_rx.try_recv().is_err(),
            "producer must be blocked while the queue is full"
        );
        cancelled.store(true, Ordering::Release);
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("cancel flag must release the blocked reliable send");
        producer.join().unwrap();
        // The pending send was aborted, not delivered. Proof via non-destructive
        // accounting: `high_water_events` rises only when an envelope actually
        // enters the queue, and the parked helper's envelope counts its reserved
        // bytes while waiting. Dequeuing to inspect the queue would free the
        // slot and let the helper's still-parked `send` deliver AFTER
        // `send_reliable` returned — that racy ordering caused the flaky
        // failure in run 41881747452.
        let held_bytes = 64 + "held".len();
        assert_eq!(
            stats_tx.stats().queued_bytes,
            held_bytes + 64,
            "held event plus the parked send's reserved bytes"
        );
        assert_eq!(stats_tx.stats().high_water_events, 1);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(stats_tx.stats().high_water_events, 1, "never delivered");
        // Closing the receiver releases the helper; its envelope drops without
        // ever entering the queue.
        drop(rx);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(stats_tx.stats().queued_bytes, 0);
        assert_eq!(stats_tx.stats().high_water_events, 1);
    }

    /// Companion: a send blocked on the BYTE semaphore (not a slot) must also
    /// be released by the cancel flag, even when the receiver stays alive.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn should_release_byte_waiter_when_turn_cancel_flag_is_set() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let (tx, rx) = stream_event_channel_with_limits(8, 128);
        let tx = tx.with_cancel_flag(Arc::clone(&cancelled));
        tx.send_reliable(StreamProgressEvent::ToolStarted {
            name: "x".repeat(64),
        });
        let stats_tx = tx.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let producer = std::thread::spawn(move || {
            tx.send_reliable(StreamProgressEvent::ToolCompleted {
                name: "y".repeat(200),
                success: true,
            });
            done_tx.send(()).unwrap();
        });
        std::thread::sleep(Duration::from_millis(50));
        assert!(done_rx.try_recv().is_err(), "byte waiter must be blocked");
        cancelled.store(true, Ordering::Release);
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("cancel flag must release the byte-blocked send");
        producer.join().unwrap();
        // Same non-destructive non-delivery assertion as the slot-blocked
        // sibling: dequeuing the held event would free its byte permits and
        // let the helper's parked acquire+send deliver late — the flake. The
        // byte-blocked helper has not even built its envelope (it parks in
        // `acquire_many_owned`), so accounting holds the held event only.
        assert_eq!(stats_tx.stats().queued_bytes, 64 + 64);
        assert_eq!(stats_tx.stats().high_water_events, 1);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(stats_tx.stats().queued_bytes, 64 + 64);
        assert_eq!(stats_tx.stats().high_water_events, 1, "never delivered");
        drop(rx);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(stats_tx.stats().queued_bytes, 0);
        assert_eq!(stats_tx.stats().high_water_events, 1);
    }

    #[test]
    fn should_make_progress_when_current_thread_producer_is_backpressured() {
        use octos_agent::progress::ProgressEvent;

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let dir = tempfile::TempDir::new().unwrap();
                let active_sessions =
                    Arc::new(RwLock::new(ActiveSessionStore::open(dir.path()).unwrap()));
                let mock = Arc::new(MockChannel {
                    send_delay: Duration::from_millis(20),
                    ..Default::default()
                });
                let channel: Arc<dyn Channel> = mock;
                let (tx, rx) = stream_event_channel_with_limits(1, 64 * 1024);
                let forwarder = spawn_stream_forwarder(
                    rx,
                    channel,
                    "!room:localhost".to_owned(),
                    None,
                    None,
                    active_sessions,
                    SessionKey::new("matrix", "!room:localhost"),
                    None,
                    None,
                    None,
                    Arc::new(HashSet::new()),
                );
                let reporter = ChannelStreamReporter::new(tx.clone());
                reporter.report(ProgressEvent::StreamChunk {
                    text: "x".repeat(CHUNK_FLUSH_BYTES),
                    iteration: 1,
                });
                reporter.report(ProgressEvent::StreamChunk {
                    text: "y".repeat(CHUNK_FLUSH_BYTES),
                    iteration: 1,
                });
                reporter.report(ProgressEvent::StreamDone { iteration: 1 });
                drop(reporter);
                drop(tx);
                let result = forwarder.await.unwrap();
                assert_eq!(result.text.len(), CHUNK_FLUSH_BYTES * 2);
                done_tx.send(()).unwrap();
            });
        });
        done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "opt-in S1 pressure benchmark; run without other parallel tests"]
    async fn s1_stream_backpressure_pressure_bench() {
        use octos_agent::progress::ProgressEvent;

        let (tx, mut rx) = stream_event_channel();
        let stats_tx = tx.clone();
        let reporter = ChannelStreamReporter::new(tx);
        let started = Instant::now();
        let consumer = tokio::spawn(async move {
            let mut text = String::new();
            let mut events = 0usize;
            while let Some(mut envelope) = rx.recv().await {
                events += 1;
                match envelope.event.take().expect("event") {
                    StreamProgressEvent::Chunk { text: chunk, .. } => text.push_str(&chunk),
                    StreamProgressEvent::StreamDone { .. } => break,
                    _ => {}
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            (events, text)
        });
        let producer = tokio::task::spawn_blocking(move || {
            for _ in 0..4096 {
                reporter.report(ProgressEvent::StreamChunk {
                    text: "x".repeat(2048),
                    iteration: 1,
                });
            }
            reporter.report(ProgressEvent::StreamDone { iteration: 1 });
        });
        producer.await.unwrap();
        let (events, text) = consumer.await.unwrap();
        let stats = stats_tx.stats();
        println!(
            "{{\"chunks\":4096,\"received_events\":{events},\"high_water_events\":{},\"high_water_mib\":{:.3},\"wall_ms\":{:.3}}}",
            stats.high_water_events,
            stats.high_water_bytes as f64 / (1024.0 * 1024.0),
            started.elapsed().as_secs_f64() * 1000.0,
        );
        assert_eq!(text.len(), 4096 * 2048);
        assert!(stats.high_water_events <= STREAM_EVENT_CAPACITY);
        assert!(stats.high_water_bytes <= STREAM_EVENT_BYTE_BUDGET);
        assert_eq!(stats.dropped_noncritical, 0);
    }
}
