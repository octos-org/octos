//! Process-wide event broadcaster for the API surface.
//!
//! Originally the SSE wire format module (M9-α-5/α-6 deleted the chat
//! SSE transport entirely — see ADR PR #830 / audit issue #845). The
//! legacy `/api/ws` text-frame handler (the last consumer of the
//! per-request `ChannelReporter`) was retired in a follow-up cleanup.
//! The JSON shape produced by [`event_to_json`] is now consumed by:
//!
//! * the harness/admin `/api/events/harness` endpoint (subscribes to
//!   the broadcaster for M7.6 swarm-dashboard live frames),
//! * the swarm dispatch / cost-attribution typed event publishers in
//!   `crate::api::swarm`,
//! * the UI Protocol v1 WS bridge in `crate::api::ui_protocol`, which
//!   reuses `event_to_json` to encode forwarded progress frames.
//!
//! No SSE wire path remains in the chat transport — every chat client
//! talks to `/api/ui-protocol/ws` exclusively.

use octos_agent::{ProgressEvent, ProgressReporter};
use tokio::sync::broadcast;

/// Producer iteration identity, opaque to clients. It does not depend on how
/// many progress events were delivered or retained by the ledger.
pub(super) fn assistant_segment_id_for_iteration(thread_id: &str, iteration: u32) -> String {
    format!("{thread_id}:assistant:iteration:{iteration}")
}

/// Process-wide broadcaster of progress events, used by the harness +
/// swarm event surfaces. Publishes pre-serialized JSON frames so
/// downstream subscribers (admin dashboard, M7.8 live gate) can forward
/// them verbatim.
pub struct EventBroadcaster {
    tx: broadcast::Sender<String>,
}

impl EventBroadcaster {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Subscribe to the event stream.
    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.tx.subscribe()
    }

    /// Send a raw pre-encoded JSON frame. Used by typed endpoints
    /// (M7.6 swarm review decision) that construct the JSON body
    /// directly instead of routing through a [`ProgressEvent`].
    /// Returns the number of receivers the frame reached (0 when no
    /// subscribers are connected — the send silently drops, matching
    /// the `report` impl).
    pub(crate) fn tx_send(&self, payload: String) -> usize {
        self.tx.send(payload).unwrap_or(0)
    }
}

impl ProgressReporter for EventBroadcaster {
    fn report(&self, event: ProgressEvent) {
        // Broadcaster is process-wide and not turn-scoped, so it cannot
        // resolve a thread_id without further plumbing. Per-request
        // consumers (e.g. the UI Protocol v1 `BoundedChannelReporter`)
        // tag every payload with their turn-bound thread_id; broadcaster
        // subscribers are debug-only and tolerate the absence.
        let json = match serde_json::to_string(&event_to_json(&event, None)) {
            Ok(j) => j,
            Err(_) => return,
        };
        // Ignore send errors (no subscribers)
        let _ = self.tx.send(json);
    }
}

/// Serialize a [`ProgressEvent`] to a JSON wire payload. When `thread_id`
/// is `Some`, every payload is tagged with the thread it belongs to.
///
/// M8.10 PR #2: strictly additive — clients that don't know `thread_id`
/// silently ignore the field. When `thread_id` is `None`, the field is
/// omitted from the payload entirely.
pub(crate) fn event_to_json(event: &ProgressEvent, thread_id: Option<&str>) -> serde_json::Value {
    let mut value = match event {
        ProgressEvent::TaskStarted { task_id } => {
            serde_json::json!({
                "type": "task_started",
                "task_id": task_id,
            })
        }
        ProgressEvent::ToolStarted {
            name,
            tool_id,
            arguments,
        } => {
            let mut value = serde_json::json!({
                "type": "tool_start",
                "tool": name,
                "tool_call_id": tool_id,
            });
            // Additive: carried so the UI-protocol mapper (`map_tool_start`)
            // and the envelope `arguments_preview` can echo the call.
            if let Some(arguments) = arguments {
                value["arguments"] = arguments.clone();
            }
            value
        }
        ProgressEvent::ToolCompleted {
            name,
            tool_id,
            success,
            output_preview,
            duration,
        } => {
            let mut value = serde_json::json!({
                "type": "tool_end",
                "tool": name,
                "tool_call_id": tool_id,
                "success": success,
                "duration_ms": duration.as_millis() as u64,
            });
            // Additive: the mapper (`map_tool_end`) lifts these onto
            // `ToolCompletedEvent` so tool cards and envelope previews can
            // show the result excerpt. Bounded HERE — the chokepoint feeding
            // both the durable notification ledger and the envelope lane —
            // because producers are not trustworthy about size (a failing
            // tool emits unbounded `e.to_string()`).
            if !output_preview.is_empty() {
                value["output_preview"] = serde_json::json!(octos_core::truncated_utf8(
                    output_preview,
                    octos_core::ui_protocol::ENVELOPE_TOOL_OUTPUT_PREVIEW_MAX,
                    "…",
                ));
            }
            value
        }
        ProgressEvent::ToolProgress {
            name,
            tool_id,
            message,
        } => {
            serde_json::json!({
                "type": "tool_progress",
                "tool": name,
                "tool_call_id": tool_id,
                "message": message,
            })
        }
        ProgressEvent::StreamChunk { text, iteration } => {
            serde_json::json!({"type": "token", "text": text, "iteration": iteration})
        }
        ProgressEvent::ReasoningChunk { text, iteration } => {
            serde_json::json!({
                "type": "reasoning_chunk",
                "text": text,
                "iteration": iteration,
            })
        }
        ProgressEvent::StreamDone { .. } => {
            serde_json::json!({"type": "stream_end"})
        }
        ProgressEvent::CostUpdate {
            session_input_tokens,
            session_output_tokens,
            session_cost,
            model,
            context_window,
            ..
        } => {
            // Always serialize the cost_update payload as an object so we
            // can attach the optional `model` identifier without
            // disturbing the historical field order. Clients without the
            // new field continue to ignore unknown keys.
            let mut payload = serde_json::json!({
                "type": "cost_update",
                "input_tokens": session_input_tokens,
                "output_tokens": session_output_tokens,
                "session_cost": session_cost,
            });
            if let Some(model) = model.as_deref() {
                payload["model"] = serde_json::Value::String(model.to_string());
            }
            // Per-model context window so clients render an honest ctx gauge
            // against the real window instead of a hardcoded default.
            if let Some(window) = context_window {
                payload["context_window"] = serde_json::json!(window);
            }
            payload
        }
        ProgressEvent::Thinking { iteration } => {
            serde_json::json!({"type": "thinking", "iteration": iteration})
        }
        ProgressEvent::AgentProgress {
            iteration,
            active_tokens,
            elapsed,
            checkpoints,
            reflecting,
        } => {
            serde_json::json!({
                "type": "agent_progress",
                "message": octos_agent::progress::agent_progress_message(
                    *iteration,
                    *active_tokens,
                    *elapsed,
                    *checkpoints,
                    *reflecting,
                ),
                "iteration": iteration,
                "active_tokens": active_tokens,
                "elapsed_ms": elapsed.as_millis() as u64,
                "checkpoints": checkpoints,
                "reflecting": reflecting,
            })
        }
        ProgressEvent::Response { iteration, .. } => {
            serde_json::json!({"type": "response", "iteration": iteration})
        }
        ProgressEvent::FileModified {
            path,
            created,
            previous_content,
        } => {
            serde_json::json!({
                "type": "file_modified",
                "path": path,
                "created": created,
                "previous_content": previous_content,
            })
        }
        ProgressEvent::PlanUpdated { plan } => {
            serde_json::json!({"type": "plan_updated", "plan": plan})
        }
        ProgressEvent::TokenUsage {
            input_tokens,
            output_tokens,
        } => {
            serde_json::json!({
                "type": "cost_update",
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
            })
        }
        ProgressEvent::TaskCompleted {
            success,
            iterations,
            duration,
        } => {
            serde_json::json!({
                "type": "task_completed",
                "success": success,
                "iterations": iterations,
                "duration_ms": duration.as_millis() as u64,
            })
        }
        ProgressEvent::TaskInterrupted { iterations } => {
            serde_json::json!({
                "type": "task_interrupted",
                "iterations": iterations,
            })
        }
        ProgressEvent::MaxIterationsReached { limit } => {
            serde_json::json!({
                "type": "max_iterations_reached",
                "limit": limit,
            })
        }
        ProgressEvent::TokenBudgetExceeded { used, limit } => {
            serde_json::json!({
                "type": "token_budget_exceeded",
                "used": used,
                "limit": limit,
            })
        }
        ProgressEvent::ActivityTimeoutReached { elapsed, limit } => {
            serde_json::json!({
                "type": "activity_timeout_reached",
                "elapsed_ms": elapsed.as_millis() as u64,
                "limit_ms": limit.as_millis() as u64,
            })
        }
        ProgressEvent::LlmStatus { message, iteration } => {
            serde_json::json!({
                "type": "llm_status",
                "message": message,
                "iteration": iteration,
            })
        }
        ProgressEvent::StreamRetry { iteration } => {
            serde_json::json!({
                "type": "stream_retry",
                "message": "stream retry",
                "iteration": iteration,
            })
        }
    };
    if let (Some(tid), Some(obj)) = (thread_id, value.as_object_mut()) {
        obj.insert(
            "thread_id".to_string(),
            serde_json::Value::String(tid.to_string()),
        );
    }
    value
}
