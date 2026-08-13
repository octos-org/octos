//! Progressive streaming reporter for messaging channels.
//!
//! Bridges the synchronous `ProgressReporter` trait to async channel I/O,
//! enabling real-time LLM text streaming to Telegram, WhatsApp, etc.
//! Text is accumulated and the channel message is edited at a throttled rate.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use octos_agent::progress::{ProgressEvent, ProgressReporter};
use octos_bus::{ActiveSessionStore, Channel};
use octos_core::{METADATA_SENDER_USER_ID, OutboundMessage, SessionKey};
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, warn};

/// Tools that already emit their own `org.octos.app` card (a plan card, an app
/// card), so the generic `tool_call` projection would duplicate their output.
const SELF_CARDING_TOOLS: &[&str] = &["update_plan", "send_app_card"];

/// Max length of a failed tool's error summary embedded in the `run_details`
/// card (surfaced only when the user expands the card).
///
/// Truncated because it enters the room event unencrypted and can carry noise
/// or secrets. Successful tools carry NO arguments/output at all — only a failed
/// tool contributes a (truncated) error, so a problem can be diagnosed on expand
/// without leaking a whole command line or `.env` on every successful call.
const TOOL_ERROR_PREVIEW_MAX: usize = 200;

/// Accumulates a turn's token/cost figures across the per-response `CostUpdate`
/// events so, on `TaskCompleted` (turn end), a single token/cost block can be
/// folded into the merged `run_details` (attached to the answer message) rather
/// than emitted per response.
#[derive(Default)]
struct RunSummaryAccum {
    model: Option<String>,
    /// Turn-cumulative token counts — the latest `CostUpdate` carries the total.
    input_tokens: u64,
    output_tokens: u64,
    /// Cumulative session cost from the latest `CostUpdate`.
    session_cost: Option<f64>,
    /// Turn cost = sum of the per-response costs seen this turn.
    turn_cost: f64,
    has_cost: bool,
}

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
    /// An `org.octos.app` agent-output card projected from a runtime event
    /// (a completed tool call, a plan update, …). The forwarder emits it as a
    /// structured Matrix card on capable clients (Robrix); other clients fall
    /// back to the `body` text. See `run_stream_forwarder`.
    AppCard {
        /// The `org.octos.app.type` key (e.g. `"tool_call"`, `"plan"`).
        card_type: String,
        /// Human-readable fallback text for clients without the type registry.
        body: String,
        /// The card's `initial_state` object.
        initial_state: serde_json::Value,
    },
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
    /// Per-turn token/cost accumulator, drained on `TaskCompleted` into the
    /// merged `run_details` card.
    run_accum: Mutex<Option<RunSummaryAccum>>,
    /// Files modified this turn (deduped), drained on `TaskCompleted` into a
    /// single `artifact` card — so N edits never post N cards.
    modified_files: Mutex<Vec<String>>,
    /// Tool invocations this turn, drained on `TaskCompleted` into the merged
    /// `run_details` object — so a research turn with N searches contributes ONE
    /// embedded detail, not N `tool_call` cards.
    tool_activity: Mutex<Vec<serde_json::Value>>,
    /// The merged `run_details` (`{tools, tokens, cost, …}`) built on
    /// `TaskCompleted`, held here for the session actor to drain via
    /// [`take_run_details`](Self::take_run_details) and attach as
    /// `org.octos.run` metadata onto the final answer message.
    run_details: Mutex<Option<serde_json::Value>>,
}

impl ChannelStreamReporter {
    pub fn new(tx: mpsc::UnboundedSender<StreamProgressEvent>) -> Self {
        Self {
            tx,
            thread_id: None,
            run_accum: Mutex::new(None),
            modified_files: Mutex::new(Vec::new()),
            tool_activity: Mutex::new(Vec::new()),
            run_details: Mutex::new(None),
        }
    }

    /// Bind a `thread_id` (typically the user message's `client_message_id`)
    /// to every SSE payload this reporter emits.
    pub fn with_thread_id(mut self, thread_id: Option<String>) -> Self {
        self.thread_id = thread_id.filter(|s| !s.is_empty());
        self
    }

    /// Take the merged `run_details` (`{tools, tokens, cost, …}`) built on the
    /// turn's `TaskCompleted`, if any. Returns `None` when the turn ran no tools.
    /// Callable through a shared handle after the agent task completes; the value
    /// is cleared so a subsequent turn starts fresh.
    pub fn take_run_details(&self) -> Option<serde_json::Value> {
        self.run_details
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
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

/// Coarse artifact kind from a file extension, feeding the `artifact` card's
/// per-item glyph on the client.
fn artifact_kind_for(path: &str) -> &'static str {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "rs" | "py" | "js" | "ts" | "go" | "c" | "cpp" | "h" | "java" | "rb" | "sh" => "code",
        "md" | "txt" | "rst" | "adoc" => "doc",
        "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" => "image",
        "json" | "csv" | "tsv" | "yaml" | "yml" | "toml" => "data",
        "log" => "log",
        _ => "file",
    }
}

/// Truncate a preview string to at most `max` characters (char-safe), trimming
/// surrounding whitespace and appending an ellipsis when cut. Bounds the tool
/// output echoed into a projected `tool_call` card.
fn truncate_card_preview(s: &str, max: usize) -> String {
    let t = s.trim();
    if t.chars().count() <= max {
        t.to_string()
    } else {
        let cut: String = t.chars().take(max).collect();
        format!("{cut}…")
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
                arguments: _,
            } => {
                // Send raw SSE for web client status indicators. Call arguments
                // are intentionally not recorded — the `run_details` card never
                // echoes them (only a failed tool's truncated error is surfaced).
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
                ref output_preview,
                duration,
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
                // Accumulate this tool into the per-turn `run_details` card (one
                // merged card at turn end) — except self-carding tools, which
                // emit their own card. A FAILED tool contributes a truncated
                // error so the failure can be diagnosed when the card is
                // expanded; a successful tool carries only name/status/duration
                // (no args/output on the wire — see `TOOL_ERROR_PREVIEW_MAX`).
                if !SELF_CARDING_TOOLS.contains(&name.as_str()) {
                    let status = if success { "completed" } else { "error" };
                    let mut entry = serde_json::json!({
                        "tool_name": name,
                        "status": status,
                        "duration_ms": duration.as_millis() as u64,
                    });
                    if !success {
                        entry["error"] = serde_json::Value::String(truncate_card_preview(
                            output_preview,
                            TOOL_ERROR_PREVIEW_MAX,
                        ));
                    }
                    if let Ok(mut v) = self.tool_activity.lock() {
                        v.push(entry);
                    }
                }
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
            ProgressEvent::FileModified { path } => {
                // Record for a coalesced `artifact` card at turn end (deduped).
                if let Ok(mut v) = self.modified_files.lock() {
                    if !v.iter().any(|p| p == &path) {
                        v.push(path.clone());
                    }
                }
                StreamProgressEvent::FileWritten { path }
            }
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
                turn_input_tokens,
                turn_output_tokens,
                response_cost,
                session_cost,
                model,
                context_window,
            } => {
                // Accumulate the turn's token/cost figures; folded into the
                // merged `run_details` on TaskCompleted (turn end).
                if let Ok(mut acc) = self.run_accum.lock() {
                    let a = acc.get_or_insert_with(RunSummaryAccum::default);
                    a.input_tokens = turn_input_tokens as u64;
                    a.output_tokens = turn_output_tokens as u64;
                    if model.is_some() {
                        a.model = model.clone();
                    }
                    if let Some(c) = session_cost {
                        a.session_cost = Some(c);
                        a.has_cost = true;
                    }
                    if let Some(c) = response_cost {
                        a.turn_cost += c;
                        a.has_cost = true;
                    }
                }
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
            ProgressEvent::PlanUpdated { plan } => {
                // `plan` is a serialized UiPlanRecord ({items, title?, …}), which
                // is exactly the `plan` card's initial_state contract.
                let _ = self.tx.send(StreamProgressEvent::AppCard {
                    card_type: "plan".to_string(),
                    body: "Plan updated".to_string(),
                    initial_state: plan,
                });
                return;
            }
            ProgressEvent::TaskCompleted { duration, .. } => {
                // Drain BOTH per-turn accumulators (always, so a turn never
                // bleeds into the next), then merge tool activity + token/cost
                // into ONE `run_details` object. Rather than posting a separate
                // card message, we STASH it so the session actor can attach it as
                // `org.octos.run` metadata onto the FINAL answer message — the
                // client renders it embedded + expandable under the answer, not
                // as its own row. Built only when the turn ran tools (a pure-text
                // answer stashes nothing). Matrix canonical JSON forbids floats,
                // so costs are decimal strings.
                let tools = self
                    .tool_activity
                    .lock()
                    .ok()
                    .map(|mut v| std::mem::take(&mut *v))
                    .unwrap_or_default();
                let run = self.run_accum.lock().ok().and_then(|mut a| a.take());
                if !tools.is_empty() {
                    let mut initial_state = serde_json::json!({
                        "tools": tools,
                        "duration_ms": duration.as_millis() as u64,
                    });
                    if let Some(acc) = run {
                        initial_state["input_tokens"] = acc.input_tokens.into();
                        initial_state["output_tokens"] = acc.output_tokens.into();
                        initial_state["total_tokens"] =
                            acc.input_tokens.saturating_add(acc.output_tokens).into();
                        if let Some(m) = acc.model {
                            initial_state["model"] = serde_json::Value::String(m);
                        }
                        if acc.has_cost {
                            initial_state["response_cost"] =
                                serde_json::Value::String(format!("{:.4}", acc.turn_cost));
                            if let Some(sc) = acc.session_cost {
                                initial_state["session_cost"] =
                                    serde_json::Value::String(format!("{:.4}", sc));
                            }
                        }
                    }
                    if let Ok(mut slot) = self.run_details.lock() {
                        *slot = Some(initial_state);
                    }
                }
                // Emit one `artifact` card listing the files produced this turn.
                if let Some(files) = self
                    .modified_files
                    .lock()
                    .ok()
                    .map(|mut v| std::mem::take(&mut *v))
                    .filter(|v| !v.is_empty())
                {
                    let artifacts: Vec<serde_json::Value> = files
                        .iter()
                        .map(|path| {
                            let filename = std::path::Path::new(path)
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or(path.as_str());
                            serde_json::json!({
                                "title": filename,
                                "kind": artifact_kind_for(path),
                                "status": "ready",
                                "path": path,
                            })
                        })
                        .collect();
                    let _ = self.tx.send(StreamProgressEvent::AppCard {
                        card_type: "artifact".to_string(),
                        body: format!("{} file(s) written", files.len()),
                        initial_state: serde_json::json!({ "artifacts": artifacts }),
                    });
                }
                return;
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
    matrix_app_reply_tools: Arc<HashSet<String>>,
    // Session workspace root — used to reconstruct `git diff` for the
    // `diff_view` card at turn end (`api` feature only).
    workspace_root: Option<std::path::PathBuf>,
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
    // Files written this turn (deduped), used to emit ONE `diff_view` card after
    // the loop (turn end), reconstructed via `git diff`. `api` feature only.
    #[cfg(feature = "api")]
    let mut diff_paths: Vec<String> = Vec::new();

    while let Some(event) = rx.recv().await {
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
            StreamProgressEvent::AppCard {
                card_type,
                body,
                initial_state,
            } => {
                // Automatic agent-output card. Only meaningful on clients that
                // understand `org.octos.app` (Robrix); other clients render the
                // `body` fallback. Gated on the same condition as transient tool
                // status suppression: matrix + an app-reply-capable tool set.
                if !suppress_matrix_transient_status {
                    continue;
                }
                if !is_session_active(&session_key, &active_sessions).await {
                    continue;
                }
                let mut metadata = serde_json::json!({
                    "org.octos.app": {
                        "type": card_type.clone(),
                        "version": 1,
                        "initial_state": initial_state,
                    }
                });
                if let Some(uid) = sender_user_id.as_deref() {
                    metadata[METADATA_SENDER_USER_ID] = serde_json::Value::String(uid.to_string());
                }
                let msg = OutboundMessage {
                    channel: channel.name().to_string(),
                    chat_id: chat_id.clone(),
                    content: body,
                    reply_to: None,
                    media: Vec::new(),
                    metadata,
                };
                if let Err(e) = channel.send(&msg).await {
                    warn!(card = %card_type, "failed to emit agent-output card: {e}");
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
                // Record (deduped) for a coalesced `diff_view` card at turn end.
                #[cfg(feature = "api")]
                if !diff_paths.contains(&path) {
                    diff_paths.push(path.clone());
                }
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

    // Turn end: emit ONE `diff_view` card for the files edited this turn, with
    // each diff reconstructed via `git diff` off the async executor. Coalesced
    // (one card, net diff), gated on the same matrix + app-reply condition, and
    // on the `api` feature (which owns the diff-reconstruction helpers).
    #[cfg(feature = "api")]
    if suppress_matrix_transient_status
        && !diff_paths.is_empty()
        && is_session_active(&session_key, &active_sessions).await
        && let Some(root) = workspace_root.clone()
    {
        let paths = std::mem::take(&mut diff_paths);
        // Match the JoinResult explicitly: a panic inside the blocking closure
        // must be logged, not silently collapsed into `None` (which is also the
        // legitimate "no diff" result), or a panic here is undiagnosable.
        let initial_state = match tokio::task::spawn_blocking(move || {
            crate::api::diff_view_for_paths(&paths, &root)
        })
        .await
        {
            Ok(state) => state,
            Err(join_err) => {
                warn!("diff_view reconstruction task panicked: {join_err}");
                None
            }
        };
        if let Some(initial_state) = initial_state {
            let mut metadata = serde_json::json!({
                "org.octos.app": {
                    "type": "diff_view",
                    "version": 1,
                    "initial_state": initial_state,
                }
            });
            if let Some(uid) = sender_user_id.as_deref() {
                metadata[METADATA_SENDER_USER_ID] = serde_json::Value::String(uid.to_string());
            }
            let msg = OutboundMessage {
                channel: channel.name().to_string(),
                chat_id: chat_id.clone(),
                content: "Diff preview".to_string(),
                reply_to: None,
                media: Vec::new(),
                metadata,
            };
            if let Err(e) = channel.send(&msg).await {
                warn!("failed to emit diff_view card: {e}");
            }
        }
    }
    #[cfg(not(feature = "api"))]
    let _ = &workspace_root;

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

    #[derive(Default)]
    struct MockChannel {
        sent: Arc<Mutex<Vec<OutboundMessage>>>,
        bound_thread_ids: Arc<Mutex<Vec<BoundCall>>>,
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
        while let Ok(event) = rx.try_recv() {
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

    /// A turn's tools (one ok, one failed) coalesce with the turn's token/cost
    /// into ONE merged `run_details` object — STASHED (via `take_run_details`)
    /// for attachment to the answer message, NOT posted as a card, and NOT split
    /// into `tool_call` / `tool_activity` / `run_summary` cards. A failed tool
    /// carries a truncated `error`; a successful tool carries no args/output. The
    /// `plan` card still fires immediately on `PlanUpdated`. Costs are decimal
    /// strings (Matrix canonical JSON forbids floats).
    #[test]
    fn projects_run_details_and_plan_app_cards() {
        use octos_agent::progress::ProgressEvent;
        use std::collections::HashMap;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let reporter = ChannelStreamReporter::new(tx);

        // Two tool calls in one turn (one ok, one error).
        reporter.report(ProgressEvent::ToolStarted {
            name: "web_search".into(),
            tool_id: "t1".into(),
            arguments: Some(serde_json::json!({ "query": "SK Hynix 2025" })),
        });
        reporter.report(ProgressEvent::ToolCompleted {
            name: "web_search".into(),
            tool_id: "t1".into(),
            success: true,
            output_preview: "Results for: SK Hynix".into(),
            duration: Duration::from_millis(3800),
        });
        reporter.report(ProgressEvent::ToolStarted {
            name: "web_fetch".into(),
            tool_id: "t2".into(),
            arguments: Some(serde_json::json!({ "url": "https://x" })),
        });
        reporter.report(ProgressEvent::ToolCompleted {
            name: "web_fetch".into(),
            tool_id: "t2".into(),
            success: false,
            output_preview: "private IP not allowed".into(),
            duration: Duration::from_millis(30),
        });
        reporter.report(ProgressEvent::CostUpdate {
            session_input_tokens: 130,
            session_output_tokens: 70,
            turn_input_tokens: 70,
            turn_output_tokens: 40,
            response_cost: Some(0.003),
            session_cost: Some(0.013),
            model: Some("deepseek-chat".into()),
            context_window: Some(64000),
        });
        reporter.report(ProgressEvent::PlanUpdated {
            plan: serde_json::json!({
                "title": "Do the thing",
                "items": [{ "id": "1", "title": "step", "status": "in_progress" }],
            }),
        });
        reporter.report(ProgressEvent::TaskCompleted {
            success: true,
            iterations: 2,
            duration: Duration::from_millis(5000),
        });

        let mut cards: HashMap<String, serde_json::Value> = HashMap::new();
        while let Ok(event) = rx.try_recv() {
            if let StreamProgressEvent::AppCard {
                card_type,
                initial_state,
                ..
            } = event
            {
                cards.insert(card_type, initial_state);
            }
        }

        // Plan card fires immediately on PlanUpdated.
        let plan = cards.get("plan").expect("plan card");
        assert_eq!(plan["items"][0]["status"].as_str(), Some("in_progress"));

        // The run detail is NOT a standalone card (it's stashed for the answer
        // message), and there are no per-tool / tool_activity / run_summary cards.
        assert!(
            !cards.contains_key("tool_call"),
            "must not emit per-tool tool_call cards"
        );
        assert!(
            !cards.contains_key("tool_activity"),
            "no tool_activity card"
        );
        assert!(!cards.contains_key("run_summary"), "no run_summary card");
        assert!(
            !cards.contains_key("run_details"),
            "run_details is stashed, not posted as a standalone card"
        );

        // The merged run detail is available for the answer message to carry.
        let state = reporter
            .take_run_details()
            .expect("run_details stashed on TaskCompleted");
        let tools = state["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 2);
        // Successful tool: name/status/duration, and NO error / NO args leak.
        assert_eq!(tools[0]["tool_name"].as_str(), Some("web_search"));
        assert_eq!(tools[0]["status"].as_str(), Some("completed"));
        assert_eq!(tools[0]["duration_ms"].as_u64(), Some(3800));
        assert!(
            tools[0].get("error").is_none(),
            "successful tool carries no error/output on the wire"
        );
        // Failed tool: a truncated error so the failure can be diagnosed on expand.
        assert_eq!(tools[1]["tool_name"].as_str(), Some("web_fetch"));
        assert_eq!(tools[1]["status"].as_str(), Some("error"));
        assert!(
            tools[1]["error"].as_str().unwrap().contains("private IP"),
            "failed tool surfaces its (truncated) error"
        );

        // Token/cost folded into the SAME detail (shown on expand).
        assert_eq!(state["model"].as_str(), Some("deepseek-chat"));
        assert_eq!(state["input_tokens"].as_u64(), Some(70));
        assert_eq!(state["output_tokens"].as_u64(), Some(40));
        assert_eq!(state["total_tokens"].as_u64(), Some(110));
        assert_eq!(state["duration_ms"].as_u64(), Some(5000));
        assert_eq!(state["response_cost"].as_str(), Some("0.0030"));
        assert_eq!(state["session_cost"].as_str(), Some("0.0130"));

        // Draining clears it so the next turn starts fresh.
        assert!(
            reporter.take_run_details().is_none(),
            "run_details is cleared after being taken"
        );
    }

    /// A `run_details` object is stashed for every turn that ran tools, and the
    /// per-turn accumulator never bleeds: turn 1 (a successful tool) and turn 2
    /// (a failing tool) each yield their OWN detail carrying only that turn's
    /// tool.
    #[test]
    fn run_details_emits_per_turn_without_bleed() {
        use octos_agent::progress::ProgressEvent;

        let (tx, _rx) = mpsc::unbounded_channel();
        let reporter = ChannelStreamReporter::new(tx);

        // Turn 1: one successful tool, then turn end.
        reporter.report(ProgressEvent::ToolStarted {
            name: "web_search".into(),
            tool_id: "t1".into(),
            arguments: None,
        });
        reporter.report(ProgressEvent::ToolCompleted {
            name: "web_search".into(),
            tool_id: "t1".into(),
            success: true,
            output_preview: "ok".into(),
            duration: Duration::from_millis(1200),
        });
        reporter.report(ProgressEvent::TaskCompleted {
            success: true,
            iterations: 1,
            duration: Duration::from_millis(1500),
        });
        let t1_state = reporter.take_run_details().expect("turn 1 run_details");
        let t1 = t1_state["tools"].as_array().expect("turn 1 tools");
        assert_eq!(t1.len(), 1);
        assert_eq!(t1[0]["tool_name"].as_str(), Some("web_search"));
        assert_eq!(t1[0]["status"].as_str(), Some("completed"));

        // Turn 2: one FAILING tool, then turn end.
        reporter.report(ProgressEvent::ToolStarted {
            name: "web_fetch".into(),
            tool_id: "t2".into(),
            arguments: None,
        });
        reporter.report(ProgressEvent::ToolCompleted {
            name: "web_fetch".into(),
            tool_id: "t2".into(),
            success: false,
            output_preview: "boom".into(),
            duration: Duration::from_millis(30),
        });
        reporter.report(ProgressEvent::TaskCompleted {
            success: true,
            iterations: 1,
            duration: Duration::from_millis(50),
        });
        let t2_state = reporter.take_run_details().expect("turn 2 run_details");
        let t2 = t2_state["tools"].as_array().expect("turn 2 tools");
        // ONLY turn 2's failing tool — turn 1 did not bleed in.
        assert_eq!(t2.len(), 1, "turn 1's tool must not bleed into turn 2");
        assert_eq!(t2[0]["tool_name"].as_str(), Some("web_fetch"));
        assert_eq!(t2[0]["status"].as_str(), Some("error"));
    }

    /// A turn with cost updates but NO tools stashes NO run detail: token/cost is
    /// telemetry that only rides along when the turn actually did tool work. The
    /// cost accumulator is still drained so it never bleeds into the next turn.
    #[test]
    fn run_details_suppressed_on_cost_only_turn() {
        use octos_agent::progress::ProgressEvent;

        let (tx, _rx) = mpsc::unbounded_channel();
        let reporter = ChannelStreamReporter::new(tx);

        reporter.report(ProgressEvent::CostUpdate {
            session_input_tokens: 100,
            session_output_tokens: 50,
            turn_input_tokens: 40,
            turn_output_tokens: 20,
            response_cost: Some(0.001),
            session_cost: Some(0.010),
            model: Some("deepseek-chat".into()),
            context_window: Some(64000),
        });
        reporter.report(ProgressEvent::TaskCompleted {
            success: true,
            iterations: 1,
            duration: Duration::from_millis(3400),
        });

        assert!(
            reporter.take_run_details().is_none(),
            "a cost-only turn (no tools) stashes no run_details"
        );
    }

    /// Files modified during a turn are deduped and coalesced into ONE
    /// `artifact` card on `TaskCompleted`, with a kind inferred per extension.
    #[test]
    fn projects_artifact_card_coalesced_on_turn_end() {
        use octos_agent::progress::ProgressEvent;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let reporter = ChannelStreamReporter::new(tx);

        reporter.report(ProgressEvent::FileModified {
            path: "src/auth/mod.rs".into(),
        });
        reporter.report(ProgressEvent::FileModified {
            path: "src/auth/mod.rs".into(), // duplicate -> one entry
        });
        reporter.report(ProgressEvent::FileModified {
            path: "README.md".into(),
        });
        reporter.report(ProgressEvent::TaskCompleted {
            success: true,
            iterations: 1,
            duration: Duration::from_millis(10),
        });

        let mut art = None;
        while let Ok(event) = rx.try_recv() {
            if let StreamProgressEvent::AppCard {
                card_type,
                initial_state,
                ..
            } = event
            {
                if card_type == "artifact" {
                    art = Some(initial_state);
                }
            }
        }
        let art = art.expect("expected an artifact card on TaskCompleted");
        let items = art["artifacts"].as_array().expect("artifacts array");
        assert_eq!(items.len(), 2, "src/auth/mod.rs deduped -> 2 files");
        assert_eq!(items[0]["title"].as_str(), Some("mod.rs"));
        assert_eq!(items[0]["kind"].as_str(), Some("code"));
        assert_eq!(items[0]["path"].as_str(), Some("src/auth/mod.rs"));
        assert_eq!(items[1]["title"].as_str(), Some("README.md"));
        assert_eq!(items[1]["kind"].as_str(), Some("doc"));
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

        let (tx, mut rx) = mpsc::unbounded_channel();
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

        let StreamProgressEvent::RawSse { json } = rx.try_recv().unwrap() else {
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

        let (tx, mut rx) = mpsc::unbounded_channel();
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

        let StreamProgressEvent::RawSse { json } = rx.try_recv().unwrap() else {
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

        let (tx, mut rx) = mpsc::unbounded_channel();
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

        let StreamProgressEvent::RawSse { json } = rx.try_recv().unwrap() else {
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
        let (tx, rx) = mpsc::unbounded_channel();

        tx.send(StreamProgressEvent::ToolStarted {
            name: "send_app_card".to_string(),
        })
        .unwrap();
        tx.send(StreamProgressEvent::ToolCompleted {
            name: "send_app_card".to_string(),
            success: true,
        })
        .unwrap();
        tx.send(StreamProgressEvent::Chunk {
            text: "hallucinated weather summary".to_string(),
            iteration: 1,
        })
        .unwrap();
        tx.send(StreamProgressEvent::StreamDone { iteration: 1 })
            .unwrap();
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
            None,
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
}
