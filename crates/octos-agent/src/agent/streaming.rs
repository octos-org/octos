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

    pub(super) async fn consume_stream_with_input_estimate(
        &self,
        stream: ChatStream,
        iteration: u32,
        input_tokens_estimate: u32,
    ) -> Result<(ChatResponse, bool)> {
        self.consume_stream_inner(stream, iteration, input_tokens_estimate)
            .await
    }

    async fn consume_stream_inner(
        &self,
        mut stream: ChatStream,
        iteration: u32,
        input_tokens_estimate: u32,
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
        let mut provider_index = None;

        // Adaptive stream timeout:
        // - TTFT (first token): generous — models need time to process large
        //   inputs before generating. Scales with input: base 30s + 1s per 1K
        //   input tokens, capped at 180s.
        // - Inter-chunk: once streaming starts, chunks arrive every <1s.
        //   If no chunk for 30s after first token, the stream is stalled.
        let ttft_secs = (30 + input_tokens_estimate as u64 / 1000).min(180);
        let mut got_first_chunk = false;

        loop {
            let timeout = if got_first_chunk {
                std::time::Duration::from_secs(30)
            } else {
                std::time::Duration::from_secs(ttft_secs)
            };

            let event = tokio::select! {
                event = stream.next() => event,
                _ = self.wait_for_shutdown() => {
                    warn!("shutdown received during streaming");
                    break;
                }
                _ = tokio::time::sleep(timeout) => {
                    if got_first_chunk {
                        warn!("stream inter-chunk timeout after 30s — provider stalled");
                    } else {
                        warn!("stream TTFT timeout after {ttft_secs}s (input_estimate={input_tokens_estimate})");
                    }
                    break;
                }
            };

            let Some(event) = event else {
                tracing::debug!("stream ended (None)");
                break;
            };
            tracing::debug!(?event, "stream event received");

            match event {
                StreamEvent::ProviderIndex(index) => {
                    provider_index = Some(index);
                }
                StreamEvent::ReasoningDelta(delta) => {
                    got_first_chunk = true;
                    reasoning.push_str(&delta);
                }
                StreamEvent::TextDelta(delta) => {
                    got_first_chunk = true;
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
                let arguments = parse_tool_call_arguments(&name, &args);
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
                provider_index,
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

fn parse_tool_call_arguments(name: &str, raw: &str) -> serde_json::Value {
    serde_json::from_str(raw).unwrap_or_else(|e| {
        if let Some(recovered) = recover_tool_call_args(name, raw) {
            tracing::info!(tool = %name, raw_len = raw.len(), "recovered malformed tool call JSON");
            return recovered;
        }
        tracing::warn!(tool = %name, error = %e, "malformed tool call JSON");
        serde_json::Value::String(format!(
            "MALFORMED_JSON: {e}. Raw input: {}",
            octos_core::truncated_utf8(raw, 200, "...")
        ))
    })
}

fn recover_tool_call_args(name: &str, raw: &str) -> Option<serde_json::Value> {
    match name {
        "write_file" => recover_write_file_args(raw),
        "fm_tts" => recover_tts_args(raw, "voice"),
        "voice_synthesize" => recover_tts_args(raw, "speaker"),
        _ => None,
    }
}

/// Recover write_file arguments from a truncated JSON string.
///
/// When the LLM's streaming output is cut off, the JSON for write_file looks like:
/// `{"path":"./report.md","content":"# Report...<truncated>`
///
/// We extract `path` and `content` fields even from broken JSON, allowing the
/// file to be written with the content we received (truncated but better than lost).
fn recover_write_file_args(raw: &str) -> Option<serde_json::Value> {
    // Try to find "path" field
    let path = extract_json_string_field(raw, "path")
        .or_else(|| extract_json_string_field(raw, "file_path"))?;

    // Try to find "content" field — it may be truncated
    let content = extract_json_string_field(raw, "content").unwrap_or_default();

    if path.is_empty() {
        return None;
    }

    // Add a truncation notice if the JSON was clearly cut off
    let content = if !raw.ends_with('}') && !content.is_empty() {
        format!(
            "{content}\n\n---\n*[Note: This report was truncated due to output length limits. The content above is partial.]*"
        )
    } else {
        content
    };

    Some(serde_json::json!({
        "path": path,
        "content": content,
    }))
}

fn recover_tts_args(raw: &str, voice_field: &str) -> Option<serde_json::Value> {
    recover_json_object_args(
        raw,
        &["text"],
        &[voice_field, "output_path", "language", "prompt"],
        &["speed"],
    )
}

fn recover_json_object_args(
    raw: &str,
    required_strings: &[&str],
    optional_strings: &[&str],
    optional_numbers: &[&str],
) -> Option<serde_json::Value> {
    let mut object = serde_json::Map::new();

    for field in required_strings {
        let value = extract_json_string_field(raw, field)?;
        if value.trim().is_empty() {
            return None;
        }
        object.insert((*field).to_string(), serde_json::Value::String(value));
    }

    for field in optional_strings {
        if let Some(value) = extract_json_string_field(raw, field) {
            if !value.is_empty() {
                object.insert((*field).to_string(), serde_json::Value::String(value));
            }
        }
    }

    for field in optional_numbers {
        if let Some(value) = extract_json_number_field(raw, field) {
            object.insert((*field).to_string(), value);
        }
    }

    Some(serde_json::Value::Object(object))
}

/// Extract a string value for a given key from potentially malformed JSON.
/// Handles JSON escaping within the string value.
fn extract_json_string_field(raw: &str, key: &str) -> Option<String> {
    // Look for "key": " or "key":"
    let patterns = [format!("\"{key}\": \""), format!("\"{key}\":\"")];

    for pattern in &patterns {
        if let Some(start) = raw.find(pattern.as_str()) {
            let value_start = start + pattern.len();
            let bytes = raw.as_bytes();
            let mut end = value_start;
            let mut escaped = false;

            // Walk through the string, handling JSON escapes
            while end < bytes.len() {
                if escaped {
                    escaped = false;
                    end += 1;
                    continue;
                }
                match bytes[end] {
                    b'\\' => {
                        escaped = true;
                        end += 1;
                    }
                    b'"' => break,
                    _ => end += 1,
                }
            }

            let raw_value = &raw[value_start..end];
            // Unescape JSON string escapes
            let unescaped = raw_value
                .replace("\\n", "\n")
                .replace("\\t", "\t")
                .replace("\\\"", "\"")
                .replace("\\\\", "\\");
            return Some(unescaped);
        }
    }
    None
}

fn extract_json_number_field(raw: &str, key: &str) -> Option<serde_json::Value> {
    let patterns = [format!("\"{key}\":"), format!("\"{key}\": ")];

    for pattern in &patterns {
        if let Some(start) = raw.find(pattern.as_str()) {
            let mut value_start = start + pattern.len();
            let bytes = raw.as_bytes();

            while value_start < bytes.len() && bytes[value_start].is_ascii_whitespace() {
                value_start += 1;
            }

            let mut value_end = value_start;
            while value_end < bytes.len()
                && matches!(
                    bytes[value_end],
                    b'0'..=b'9' | b'.' | b'-' | b'+' | b'e' | b'E'
                )
            {
                value_end += 1;
            }

            if value_end == value_start {
                continue;
            }

            if let Ok(value) =
                serde_json::from_str::<serde_json::Value>(&raw[value_start..value_end])
            {
                if value.is_number() {
                    return Some(value);
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tool_call_arguments_recovers_truncated_fm_tts_payload() {
        let raw =
            r#"{"text":"北京今日热点播客第一段","voice":"yangmi","prompt":"专业大方","speed":1.05"#;
        let arguments = parse_tool_call_arguments("fm_tts", raw);

        assert!(
            arguments.is_object(),
            "expected recovered object, got {arguments}"
        );
        assert_eq!(
            arguments.get("text").and_then(|v| v.as_str()),
            Some("北京今日热点播客第一段")
        );
        assert_eq!(
            arguments.get("voice").and_then(|v| v.as_str()),
            Some("yangmi")
        );
        assert_eq!(
            arguments.get("prompt").and_then(|v| v.as_str()),
            Some("专业大方")
        );
        assert_eq!(arguments.get("speed").and_then(|v| v.as_f64()), Some(1.05));
    }

    #[test]
    fn parse_tool_call_arguments_recovers_text_only_truncated_tts_payload() {
        let raw = r#"{"text":"这里是一个非常长的播客片段，"#;
        let arguments = parse_tool_call_arguments("fm_tts", raw);

        assert!(
            arguments.is_object(),
            "expected recovered object, got {arguments}"
        );
        assert_eq!(
            arguments.get("text").and_then(|v| v.as_str()),
            Some("这里是一个非常长的播客片段，")
        );
    }

    #[test]
    fn parse_tool_call_arguments_preserves_write_file_recovery() {
        let raw = r##"{"path":"./report.md","content":"# Report"##;
        let arguments = parse_tool_call_arguments("write_file", raw);

        assert!(
            arguments.is_object(),
            "expected recovered object, got {arguments}"
        );
        assert_eq!(
            arguments.get("path").and_then(|v| v.as_str()),
            Some("./report.md")
        );
        assert!(
            arguments
                .get("content")
                .and_then(|v| v.as_str())
                .is_some_and(|content| content.contains("# Report"))
        );
    }
}
