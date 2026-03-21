//! Stream consumption, shutdown handling, and cost reporting.

use std::sync::atomic::Ordering;

use eyre::Result;
use futures::StreamExt;
use octos_core::{Message, MessageRole, TokenUsage};
use octos_llm::{ChatResponse, ChatStream, StopReason, StreamEvent};
use tracing::warn;

use super::Agent;
use crate::progress::ProgressEvent;

impl Agent {
    /// Wait until the shutdown flag is set. Used with `tokio::select!`
    /// to cancel long-running operations on Ctrl+C.
    /// Returns after the flag is set OR after 30 seconds (safety guard).
    pub(super) async fn wait_for_shutdown(&self) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if self.shutdown.load(Ordering::Acquire) {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!("wait_for_shutdown: 30s deadline reached without shutdown signal");
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    pub(super) async fn consume_stream(
        &self,
        mut stream: ChatStream,
        iteration: u32,
    ) -> Result<(ChatResponse, bool)> {
        // Clear any pending status line (e.g., "Thinking...")
        self.reporter().report(ProgressEvent::Response {
            content: String::new(),
            iteration,
        });

        let mut text = String::new();
        let mut reasoning = String::new();
        // (id, name, args_json, metadata)
        let mut tool_calls: Vec<(String, String, String, Option<serde_json::Value>)> = Vec::new();
        let mut usage = octos_llm::TokenUsage::default();
        let mut stop_reason = StopReason::EndTurn;

        // Per-chunk timeout: if no SSE event arrives within 30s, the stream
        // is likely stalled (connection alive but provider stopped sending).
        // Normal chunk intervals are <1s; thinking models may pause up to ~15s
        // before the first token. 30s gives ample margin while catching stalls
        // much faster than the previous behavior (hung indefinitely).
        const CHUNK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

        loop {
            let event = tokio::select! {
                event = stream.next() => event,
                _ = self.wait_for_shutdown() => {
                    warn!("shutdown received during streaming");
                    break;
                }
                _ = tokio::time::sleep(CHUNK_TIMEOUT) => {
                    warn!("stream chunk timeout after {}s — provider may be stalled",
                        CHUNK_TIMEOUT.as_secs());
                    break;
                }
            };

            let Some(event) = event else {
                tracing::debug!("stream ended (None)");
                break;
            };
            tracing::debug!(?event, "stream event received");

            match event {
                StreamEvent::ReasoningDelta(delta) => {
                    reasoning.push_str(&delta);
                }
                StreamEvent::TextDelta(delta) => {
                    self.reporter().report(ProgressEvent::StreamChunk {
                        text: delta.clone(),
                        iteration,
                    });
                    text.push_str(&delta);
                }
                StreamEvent::ToolCallDelta {
                    index,
                    id,
                    name,
                    arguments_delta,
                } => {
                    while tool_calls.len() <= index {
                        tool_calls.push((String::new(), String::new(), String::new(), None));
                    }
                    if let Some(id) = id {
                        tool_calls[index].0 = id;
                    }
                    if let Some(name) = name {
                        tool_calls[index].1 = name;
                    }
                    tool_calls[index].2.push_str(&arguments_delta);
                }
                StreamEvent::ToolCallMetadata { index, metadata } => {
                    while tool_calls.len() <= index {
                        tool_calls.push((String::new(), String::new(), String::new(), None));
                    }
                    tool_calls[index].3 = Some(metadata);
                }
                StreamEvent::Usage(u) => {
                    usage = u;
                }
                StreamEvent::Done(reason) => {
                    stop_reason = reason;
                }
                StreamEvent::Error(err) => {
                    eyre::bail!("Stream error: {}", err);
                }
            }
        }

        let streamed = !text.is_empty();
        if streamed {
            self.reporter()
                .report(ProgressEvent::StreamDone { iteration });
        }

        // Strip <think> tags from accumulated streaming content (some models
        // embed chain-of-thought in <think> tags via TextDelta instead of
        // using ReasoningDelta events).
        let (text, think_extracted) = octos_llm::strip_think_tags(&text);
        if let Some(ref extracted) = think_extracted {
            if reasoning.is_empty() {
                reasoning = extracted.clone();
            }
        }

        let content = if text.is_empty() { None } else { Some(text) };
        let tool_calls: Vec<octos_core::ToolCall> = tool_calls
            .into_iter()
            .filter(|(_, name, _, _)| !name.is_empty())
            .map(|(id, name, args, metadata)| {
                let arguments = serde_json::from_str(&args).unwrap_or_else(|e| {
                    tracing::warn!(tool = %name, error = %e, raw = %args, "malformed tool call JSON");
                    // Return a String value so the tool's deserialize step fails
                    // and the error propagates back to the LLM for correction.
                    serde_json::Value::String(format!(
                        "MALFORMED_JSON: {e}. Raw input: {}",
                        octos_core::truncated_utf8(&args, 200, "...")
                    ))
                });
                octos_core::ToolCall {
                    id,
                    name,
                    arguments,
                    metadata,
                }
            })
            .collect();

        let reasoning_content = if reasoning.is_empty() {
            None
        } else {
            Some(reasoning)
        };

        // Fix stop_reason mismatch: some models report "stop" / EndTurn even
        // when they produced tool_calls (documented for OpenAI, Gemini).
        if !tool_calls.is_empty() && stop_reason == StopReason::EndTurn {
            tracing::warn!(
                tool_count = tool_calls.len(),
                "fixing stop_reason: EndTurn with tool_calls present -> ToolUse"
            );
            stop_reason = StopReason::ToolUse;
        }

        // Detect repetitive/looping output -- model got stuck repeating itself.
        // Replace with a short message so the user sees something useful.
        let content = if let Some(ref text) = content {
            if Self::is_repetitive_output(text) {
                tracing::warn!(
                    content_len = text.len(),
                    "detected repetitive LLM output, replacing with error message"
                );
                None
            } else {
                content
            }
        } else {
            content
        };

        Ok((
            ChatResponse {
                content,
                reasoning_content,
                tool_calls,
                stop_reason,
                usage,
            },
            streamed,
        ))
    }

    pub(super) fn emit_cost_update(
        &self,
        total_usage: &TokenUsage,
        response_usage: &octos_llm::TokenUsage,
    ) {
        let pricing = octos_llm::pricing::model_pricing(self.llm.model_id());
        let response_cost =
            pricing.map(|p| p.cost(response_usage.input_tokens, response_usage.output_tokens));
        let session_cost =
            pricing.map(|p| p.cost(total_usage.input_tokens, total_usage.output_tokens));
        self.reporter().report(ProgressEvent::CostUpdate {
            session_input_tokens: total_usage.input_tokens,
            session_output_tokens: total_usage.output_tokens,
            response_cost,
            session_cost,
        });
    }

    pub(super) fn response_to_message(&self, response: &ChatResponse) -> Message {
        Message {
            role: MessageRole::Assistant,
            content: response.content.clone().unwrap_or_default(),
            media: vec![],
            tool_calls: if response.tool_calls.is_empty() {
                None
            } else {
                Some(response.tool_calls.clone())
            },
            tool_call_id: None,
            reasoning_content: response.reasoning_content.clone(),
            timestamp: chrono::Utc::now(),
        }
    }
}
