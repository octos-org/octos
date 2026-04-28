//! SSE broadcaster for streaming progress events.

use octos_agent::{ProgressEvent, ProgressReporter};
use tokio::sync::broadcast;
use tracing::debug;

/// Broadcasts progress events to SSE subscribers.
pub struct SseBroadcaster {
    tx: broadcast::Sender<String>,
}

impl SseBroadcaster {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Subscribe to the event stream.
    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.tx.subscribe()
    }

    /// Send a raw pre-encoded JSON frame. Used by typed endpoints
    /// (M7.6 swarm review decision) that construct the SSE body
    /// directly instead of routing through a [`ProgressEvent`].
    /// Returns the number of receivers the frame reached (0 when no
    /// subscribers are connected — the send silently drops, matching
    /// the `report` impl).
    pub(crate) fn tx_send(&self, payload: String) -> usize {
        self.tx.send(payload).unwrap_or(0)
    }
}

impl ProgressReporter for SseBroadcaster {
    fn report(&self, event: ProgressEvent) {
        // Broadcaster is process-wide and not turn-scoped, so it cannot
        // resolve a thread_id without further plumbing. Per-request
        // [`ChannelReporter`] consumers receive the field via their
        // turn-bound thread_id; broadcaster subscribers are debug-only
        // and tolerate the absence.
        let json = match serde_json::to_string(&event_to_json(&event, None)) {
            Ok(j) => j,
            Err(_) => return,
        };
        // Ignore send errors (no subscribers)
        let _ = self.tx.send(json);
    }
}

/// Per-request reporter that sends serialized SSE events through an mpsc channel.
/// Used by the streaming POST /api/chat handler to isolate events per request.
///
/// M8.10 PR #2: optionally carries a `thread_id` (the user message's
/// `client_message_id`) so every emitted SSE payload is tagged with the
/// thread it belongs to. When unset, the field is omitted (legacy clients
/// continue to ignore it).
pub(crate) struct ChannelReporter {
    tx: tokio::sync::mpsc::UnboundedSender<String>,
    thread_id: Option<String>,
}

impl ChannelReporter {
    pub fn new(tx: tokio::sync::mpsc::UnboundedSender<String>) -> Self {
        Self {
            tx,
            thread_id: None,
        }
    }

    /// Bind a `thread_id` to every payload this reporter emits.
    pub fn with_thread_id(mut self, thread_id: Option<String>) -> Self {
        self.thread_id = thread_id.filter(|s| !s.is_empty());
        self
    }
}

impl ProgressReporter for ChannelReporter {
    fn report(&self, event: ProgressEvent) {
        let json = match serde_json::to_string(&event_to_json(&event, self.thread_id.as_deref())) {
            Ok(j) => j,
            Err(_) => return,
        };
        let _ = self.tx.send(json);
    }
}

/// Serialize a [`ProgressEvent`] to a JSON SSE payload. When `thread_id` is
/// `Some`, every payload is tagged with the thread it belongs to.
///
/// M8.10 PR #2: strictly additive — clients that don't know `thread_id`
/// silently ignore the field. When `thread_id` is `None`, the field is
/// omitted from the payload entirely.
pub(crate) fn event_to_json(event: &ProgressEvent, thread_id: Option<&str>) -> serde_json::Value {
    let mut value = match event {
        ProgressEvent::ToolStarted { name, tool_id } => {
            serde_json::json!({
                "type": "tool_start",
                "tool": name,
                "tool_call_id": tool_id,
            })
        }
        ProgressEvent::ToolCompleted {
            name,
            tool_id,
            success,
            ..
        } => {
            serde_json::json!({
                "type": "tool_end",
                "tool": name,
                "tool_call_id": tool_id,
                "success": success,
            })
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
        ProgressEvent::StreamChunk { text, .. } => {
            serde_json::json!({"type": "token", "text": text})
        }
        ProgressEvent::StreamDone { .. } => {
            serde_json::json!({"type": "stream_end"})
        }
        ProgressEvent::CostUpdate {
            session_input_tokens,
            session_output_tokens,
            session_cost,
            ..
        } => {
            serde_json::json!({
                "type": "cost_update",
                "input_tokens": session_input_tokens,
                "output_tokens": session_output_tokens,
                "session_cost": session_cost,
            })
        }
        ProgressEvent::Thinking { iteration } => {
            serde_json::json!({"type": "thinking", "iteration": iteration})
        }
        ProgressEvent::Response { iteration, .. } => {
            serde_json::json!({"type": "response", "iteration": iteration})
        }
        other => {
            debug!("unmapped SSE progress event: {other:?}");
            serde_json::json!({"type": "other"})
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn event_to_json_tool_started() {
        let event = ProgressEvent::ToolStarted {
            name: "shell".into(),
            tool_id: "t1".into(),
        };
        let json = event_to_json(&event, None);
        assert_eq!(json["type"], "tool_start");
        assert_eq!(json["tool"], "shell");
        assert_eq!(json["tool_call_id"], "t1");
    }

    #[test]
    fn event_to_json_tool_completed() {
        let event = ProgressEvent::ToolCompleted {
            name: "read_file".into(),
            tool_id: "t2".into(),
            success: true,
            output_preview: "contents".into(),
            duration: Duration::from_millis(42),
        };
        let json = event_to_json(&event, None);
        assert_eq!(json["type"], "tool_end");
        assert_eq!(json["tool"], "read_file");
        assert_eq!(json["tool_call_id"], "t2");
        assert_eq!(json["success"], true);
    }

    #[test]
    fn event_to_json_tool_completed_failure() {
        let event = ProgressEvent::ToolCompleted {
            name: "shell".into(),
            tool_id: "t3".into(),
            success: false,
            output_preview: "error".into(),
            duration: Duration::from_secs(1),
        };
        let json = event_to_json(&event, None);
        assert_eq!(json["success"], false);
    }

    #[test]
    fn event_to_json_tool_progress_includes_tool_call_id() {
        let event = ProgressEvent::ToolProgress {
            name: "run_pipeline".into(),
            tool_id: "call_00_XXX".into(),
            message: "plan_and_search_task_3 [...]: running deep_search".into(),
        };
        let json = event_to_json(&event, None);
        assert_eq!(json["type"], "tool_progress");
        assert_eq!(json["tool"], "run_pipeline");
        assert_eq!(json["tool_call_id"], "call_00_XXX");
        assert_eq!(
            json["message"],
            "plan_and_search_task_3 [...]: running deep_search"
        );
    }

    #[test]
    fn event_to_json_stream_chunk() {
        let event = ProgressEvent::StreamChunk {
            text: "Hello".into(),
            iteration: 1,
        };
        let json = event_to_json(&event, None);
        assert_eq!(json["type"], "token");
        assert_eq!(json["text"], "Hello");
    }

    #[test]
    fn event_to_json_stream_done() {
        let event = ProgressEvent::StreamDone { iteration: 2 };
        let json = event_to_json(&event, None);
        assert_eq!(json["type"], "stream_end");
    }

    #[test]
    fn event_to_json_cost_update() {
        let event = ProgressEvent::CostUpdate {
            session_input_tokens: 100,
            session_output_tokens: 50,
            response_cost: Some(0.001),
            session_cost: Some(0.005),
        };
        let json = event_to_json(&event, None);
        assert_eq!(json["type"], "cost_update");
        assert_eq!(json["input_tokens"], 100);
        assert_eq!(json["output_tokens"], 50);
        assert_eq!(json["session_cost"], 0.005);
    }

    #[test]
    fn event_to_json_cost_update_no_cost() {
        let event = ProgressEvent::CostUpdate {
            session_input_tokens: 200,
            session_output_tokens: 100,
            response_cost: None,
            session_cost: None,
        };
        let json = event_to_json(&event, None);
        assert_eq!(json["type"], "cost_update");
        assert!(json["session_cost"].is_null());
    }

    #[test]
    fn event_to_json_thinking() {
        let event = ProgressEvent::Thinking { iteration: 3 };
        let json = event_to_json(&event, None);
        assert_eq!(json["type"], "thinking");
        assert_eq!(json["iteration"], 3);
    }

    #[test]
    fn event_to_json_response() {
        let event = ProgressEvent::Response {
            content: "answer".into(),
            iteration: 1,
        };
        let json = event_to_json(&event, None);
        assert_eq!(json["type"], "response");
        assert_eq!(json["iteration"], 1);
    }

    #[test]
    fn event_to_json_unmapped_returns_other() {
        let event = ProgressEvent::TaskStarted {
            task_id: "abc".into(),
        };
        let json = event_to_json(&event, None);
        assert_eq!(json["type"], "other");
    }

    /// M8.10 PR #2: every SSE payload tagged with the bound thread_id so
    /// the web client can route to the right per-cmid thread bubble.
    #[test]
    fn event_to_json_includes_thread_id_when_provided() {
        let cases: &[(ProgressEvent, &str)] = &[
            (
                ProgressEvent::ToolStarted {
                    name: "shell".into(),
                    tool_id: "t1".into(),
                },
                "tool_start",
            ),
            (
                ProgressEvent::ToolCompleted {
                    name: "shell".into(),
                    tool_id: "t1".into(),
                    success: true,
                    output_preview: "ok".into(),
                    duration: Duration::from_millis(1),
                },
                "tool_end",
            ),
            (
                ProgressEvent::ToolProgress {
                    name: "shell".into(),
                    tool_id: "t1".into(),
                    message: "step".into(),
                },
                "tool_progress",
            ),
            (
                ProgressEvent::StreamChunk {
                    text: "x".into(),
                    iteration: 0,
                },
                "token",
            ),
            (ProgressEvent::StreamDone { iteration: 0 }, "stream_end"),
            (
                ProgressEvent::CostUpdate {
                    session_input_tokens: 0,
                    session_output_tokens: 0,
                    response_cost: None,
                    session_cost: None,
                },
                "cost_update",
            ),
            (ProgressEvent::Thinking { iteration: 0 }, "thinking"),
            (
                ProgressEvent::Response {
                    content: "c".into(),
                    iteration: 0,
                },
                "response",
            ),
        ];

        for (event, expected_type) in cases {
            let json = event_to_json(event, Some("cmid-T-thread"));
            assert_eq!(json["type"], *expected_type);
            assert_eq!(
                json.get("thread_id").and_then(|v| v.as_str()),
                Some("cmid-T-thread"),
                "event with type `{expected_type}` missing thread_id field, got {json}",
            );
        }
    }

    #[test]
    fn event_to_json_omits_thread_id_when_absent() {
        let json = event_to_json(&ProgressEvent::Thinking { iteration: 0 }, None);
        assert!(
            json.get("thread_id").is_none(),
            "thread_id must be absent when caller passes None, got {json}"
        );
    }

    #[test]
    fn channel_reporter_with_thread_id_tags_emitted_payloads() {
        use tokio::sync::mpsc;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let reporter = ChannelReporter::new(tx).with_thread_id(Some("cmid-route-XYZ".to_string()));

        reporter.report(ProgressEvent::Thinking { iteration: 0 });
        let raw = rx.try_recv().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["type"], "thinking");
        assert_eq!(parsed["thread_id"], "cmid-route-XYZ");
    }

    #[test]
    fn broadcaster_subscribe_receives_events() {
        let broadcaster = SseBroadcaster::new(16);
        let mut rx = broadcaster.subscribe();

        broadcaster.report(ProgressEvent::Thinking { iteration: 1 });

        let msg = rx.try_recv().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(parsed["type"], "thinking");
    }
}
