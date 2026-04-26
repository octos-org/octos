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
        let json = match serde_json::to_string(&event_to_json(&event)) {
            Ok(j) => j,
            Err(_) => return,
        };
        // Ignore send errors (no subscribers)
        let _ = self.tx.send(json);
    }
}

/// Per-request reporter that sends serialized SSE events through an mpsc channel.
/// Used by the streaming POST /api/chat handler to isolate events per request.
pub(crate) struct ChannelReporter {
    tx: tokio::sync::mpsc::UnboundedSender<String>,
}

impl ChannelReporter {
    pub fn new(tx: tokio::sync::mpsc::UnboundedSender<String>) -> Self {
        Self { tx }
    }
}

impl ProgressReporter for ChannelReporter {
    fn report(&self, event: ProgressEvent) {
        let json = match serde_json::to_string(&event_to_json(&event)) {
            Ok(j) => j,
            Err(_) => return,
        };
        let _ = self.tx.send(json);
    }
}

pub(crate) fn event_to_json(event: &ProgressEvent) -> serde_json::Value {
    match event {
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
    }
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
        let json = event_to_json(&event);
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
        let json = event_to_json(&event);
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
        let json = event_to_json(&event);
        assert_eq!(json["success"], false);
    }

    #[test]
    fn event_to_json_tool_progress_includes_tool_call_id() {
        let event = ProgressEvent::ToolProgress {
            name: "run_pipeline".into(),
            tool_id: "call_00_XXX".into(),
            message: "plan_and_search_task_3 [...]: running deep_search".into(),
        };
        let json = event_to_json(&event);
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
        let json = event_to_json(&event);
        assert_eq!(json["type"], "token");
        assert_eq!(json["text"], "Hello");
    }

    #[test]
    fn event_to_json_stream_done() {
        let event = ProgressEvent::StreamDone { iteration: 2 };
        let json = event_to_json(&event);
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
        let json = event_to_json(&event);
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
        let json = event_to_json(&event);
        assert_eq!(json["type"], "cost_update");
        assert!(json["session_cost"].is_null());
    }

    #[test]
    fn event_to_json_thinking() {
        let event = ProgressEvent::Thinking { iteration: 3 };
        let json = event_to_json(&event);
        assert_eq!(json["type"], "thinking");
        assert_eq!(json["iteration"], 3);
    }

    #[test]
    fn event_to_json_response() {
        let event = ProgressEvent::Response {
            content: "answer".into(),
            iteration: 1,
        };
        let json = event_to_json(&event);
        assert_eq!(json["type"], "response");
        assert_eq!(json["iteration"], 1);
    }

    #[test]
    fn event_to_json_unmapped_returns_other() {
        let event = ProgressEvent::TaskStarted {
            task_id: "abc".into(),
        };
        let json = event_to_json(&event);
        assert_eq!(json["type"], "other");
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
