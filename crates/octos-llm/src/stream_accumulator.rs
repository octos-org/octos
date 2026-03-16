//! Accumulates streaming events into a complete `ChatResponse`.
//!
//! Providers can use this to avoid duplicating response-building logic
//! when converting a stream of `StreamEvent`s into a final `ChatResponse`.

use octos_core::ToolCall;

use crate::types::{ChatResponse, StopReason, StreamEvent, TokenUsage};

/// Accumulates `StreamEvent`s into a final `ChatResponse`.
#[derive(Debug, Default)]
pub struct StreamAccumulator {
    text: String,
    reasoning: String,
    tool_calls: Vec<ToolCallBuilder>,
    usage: TokenUsage,
    stop_reason: Option<StopReason>,
}

#[derive(Debug, Default)]
struct ToolCallBuilder {
    id: String,
    name: String,
    arguments: String,
}

impl StreamAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Process a single stream event.
    pub fn process(&mut self, event: &StreamEvent) {
        match event {
            StreamEvent::TextDelta(text) => self.text.push_str(text),
            StreamEvent::ReasoningDelta(text) => self.reasoning.push_str(text),
            StreamEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta,
            } => {
                // Grow the tool_calls vec if needed
                while self.tool_calls.len() <= *index {
                    self.tool_calls.push(ToolCallBuilder::default());
                }
                let tc = &mut self.tool_calls[*index];
                if let Some(id) = id {
                    tc.id.clone_from(id);
                }
                if let Some(name) = name {
                    tc.name.clone_from(name);
                }
                tc.arguments.push_str(arguments_delta);
            }
            // Only the last Usage event is retained (providers emit final totals at stream end).
            StreamEvent::Usage(u) => self.usage = u.clone(),
            StreamEvent::Done(reason) => self.stop_reason = Some(*reason),
            StreamEvent::ToolCallMetadata { .. } | StreamEvent::Error(_) => {}
        }
    }

    /// Consume the accumulator and produce a `ChatResponse`.
    pub fn finish(self) -> ChatResponse {
        let tool_calls: Vec<ToolCall> = self
            .tool_calls
            .into_iter()
            .map(|tc| {
                let arguments = serde_json::from_str(&tc.arguments)
                    .unwrap_or(serde_json::Value::String(tc.arguments));
                ToolCall {
                    id: tc.id,
                    name: tc.name,
                    arguments,
                    metadata: None,
                }
            })
            .collect();

        let stop_reason = self.stop_reason.unwrap_or(if tool_calls.is_empty() {
            StopReason::EndTurn
        } else {
            StopReason::ToolUse
        });

        ChatResponse {
            content: if self.text.is_empty() {
                None
            } else {
                Some(self.text)
            },
            reasoning_content: if self.reasoning.is_empty() {
                None
            } else {
                Some(self.reasoning)
            },
            tool_calls,
            stop_reason,
            usage: self.usage,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_accumulate_text_deltas() {
        let mut acc = StreamAccumulator::new();
        acc.process(&StreamEvent::TextDelta("Hello ".into()));
        acc.process(&StreamEvent::TextDelta("world".into()));
        acc.process(&StreamEvent::Done(StopReason::EndTurn));
        let resp = acc.finish();
        assert_eq!(resp.content.as_deref(), Some("Hello world"));
        assert_eq!(resp.stop_reason, StopReason::EndTurn);
    }

    #[test]
    fn should_accumulate_reasoning_deltas() {
        let mut acc = StreamAccumulator::new();
        acc.process(&StreamEvent::ReasoningDelta("think ".into()));
        acc.process(&StreamEvent::ReasoningDelta("hard".into()));
        acc.process(&StreamEvent::TextDelta("answer".into()));
        let resp = acc.finish();
        assert_eq!(resp.reasoning_content.as_deref(), Some("think hard"));
        assert_eq!(resp.content.as_deref(), Some("answer"));
    }

    #[test]
    fn should_accumulate_tool_calls() {
        let mut acc = StreamAccumulator::new();
        acc.process(&StreamEvent::ToolCallDelta {
            index: 0,
            id: Some("call_1".into()),
            name: Some("shell".into()),
            arguments_delta: r#"{"com"#.into(),
        });
        acc.process(&StreamEvent::ToolCallDelta {
            index: 0,
            id: None,
            name: None,
            arguments_delta: r#"mand":"ls"}"#.into(),
        });
        acc.process(&StreamEvent::Done(StopReason::ToolUse));
        let resp = acc.finish();
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].name, "shell");
        assert_eq!(resp.tool_calls[0].arguments["command"], "ls");
        assert_eq!(resp.stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn should_capture_usage() {
        let mut acc = StreamAccumulator::new();
        acc.process(&StreamEvent::Usage(TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            reasoning_tokens: 10,
            ..Default::default()
        }));
        let resp = acc.finish();
        assert_eq!(resp.usage.input_tokens, 100);
        assert_eq!(resp.usage.output_tokens, 50);
        assert_eq!(resp.usage.reasoning_tokens, 10);
    }

    #[test]
    fn should_default_stop_reason_for_tool_calls() {
        let mut acc = StreamAccumulator::new();
        acc.process(&StreamEvent::ToolCallDelta {
            index: 0,
            id: Some("c1".into()),
            name: Some("read".into()),
            arguments_delta: "{}".into(),
        });
        // No Done event
        let resp = acc.finish();
        assert_eq!(resp.stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn should_default_stop_reason_end_turn() {
        let mut acc = StreamAccumulator::new();
        acc.process(&StreamEvent::TextDelta("hi".into()));
        let resp = acc.finish();
        assert_eq!(resp.stop_reason, StopReason::EndTurn);
    }

    #[test]
    fn should_handle_multiple_tool_calls() {
        let mut acc = StreamAccumulator::new();
        acc.process(&StreamEvent::ToolCallDelta {
            index: 0,
            id: Some("c0".into()),
            name: Some("read".into()),
            arguments_delta: r#"{"path":"a.rs"}"#.into(),
        });
        acc.process(&StreamEvent::ToolCallDelta {
            index: 1,
            id: Some("c1".into()),
            name: Some("read".into()),
            arguments_delta: r#"{"path":"b.rs"}"#.into(),
        });
        let resp = acc.finish();
        assert_eq!(resp.tool_calls.len(), 2);
        assert_eq!(resp.tool_calls[0].id, "c0");
        assert_eq!(resp.tool_calls[1].id, "c1");
    }
}
