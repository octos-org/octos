//! Server-Sent Events (SSE) line parser for streaming LLM responses.

use futures::stream::{self, Stream, StreamExt};

/// Maximum SSE buffer size (1 MB). Prevents unbounded memory growth from
/// a malicious or buggy server that never sends event delimiters.
const MAX_BUFFER_SIZE: usize = 1024 * 1024;

/// A parsed SSE event with optional event type and data.
#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Parse SSE events from a reqwest response using its bytes_stream().
///
/// Uses a byte buffer to avoid corrupting multi-byte UTF-8 characters that
/// may be split across HTTP chunks. UTF-8 conversion happens only after
/// complete SSE event blocks (delimited by `\n\n`) are extracted.
pub fn parse_sse_response(response: reqwest::Response) -> impl Stream<Item = SseEvent> + Send {
    let byte_stream = response.bytes_stream();
    stream::unfold(
        (Box::pin(byte_stream), Vec::<u8>::new()),
        |(mut stream, mut buffer)| async move {
            loop {
                if let Some(event) = try_extract_event_bytes(&mut buffer) {
                    return Some((event, (stream, buffer)));
                }

                match stream.next().await {
                    Some(Ok(bytes)) => {
                        buffer.extend_from_slice(&bytes);
                        if buffer.len() > MAX_BUFFER_SIZE {
                            let error = SseEvent {
                                event: None,
                                data: format!(
                                    "{{\"error\":\"SSE buffer exceeded {} bytes\"}}",
                                    MAX_BUFFER_SIZE
                                ),
                            };
                            buffer.clear();
                            return Some((error, (stream, buffer)));
                        }
                    }
                    Some(Err(e)) => {
                        tracing::warn!("SSE stream error: {e}");
                        let error = SseEvent {
                            event: Some("error".to_string()),
                            data: format!("{{\"error\":\"Stream error: {e}\"}}"),
                        };
                        return Some((error, (stream, buffer)));
                    }
                    None => {
                        tracing::debug!(remaining_buffer = buffer.len(), "SSE byte stream ended");
                        if !buffer.is_empty() {
                            let block =
                                String::from_utf8_lossy(&std::mem::take(&mut buffer)).to_string();
                            let trimmed = block.trim();
                            if !trimmed.is_empty() {
                                if let Some(event) = parse_event_block(trimmed) {
                                    return Some((event, (stream, buffer)));
                                }
                            }
                        }
                        return None;
                    }
                }
            }
        },
    )
}

/// Parse SSE events from raw string chunks (for testing and non-reqwest streams).
pub fn parse_sse_strings(
    chunks: impl Stream<Item = String> + Send + 'static,
) -> impl Stream<Item = SseEvent> + Send {
    stream::unfold(
        (Box::pin(chunks), String::new()),
        |(mut stream, mut buffer)| async move {
            loop {
                if let Some(event) = try_extract_event(&mut buffer) {
                    return Some((event, (stream, buffer)));
                }

                match stream.next().await {
                    Some(chunk) => {
                        buffer.push_str(&chunk);
                    }
                    None => {
                        if !buffer.trim().is_empty() {
                            let block = std::mem::take(&mut buffer);
                            if let Some(event) = parse_event_block(&block) {
                                return Some((event, (stream, buffer)));
                            }
                        }
                        return None;
                    }
                }
            }
        },
    )
}

/// Try to extract one complete SSE event from a byte buffer.
///
/// Searches for event delimiters (`\n\n` or `\r\n\r\n`) in raw bytes, then
/// converts only the extracted block to a UTF-8 string. This prevents
/// multi-byte characters (e.g. CJK) split across HTTP chunks from being
/// corrupted by premature `from_utf8_lossy` calls.
fn try_extract_event_bytes(buffer: &mut Vec<u8>) -> Option<SseEvent> {
    loop {
        let mut found = false;
        for sep in [b"\n\n".as_slice(), b"\r\n\r\n".as_slice()] {
            if let Some(pos) = find_bytes(buffer, sep) {
                let event_bytes = buffer[..pos].to_vec();
                let rest_start = pos + sep.len();
                *buffer = buffer[rest_start..].to_vec();

                let event_block = String::from_utf8_lossy(&event_bytes);
                if let Some(event) = parse_event_block(&event_block) {
                    return Some(event);
                }
                found = true;
                break;
            }
        }
        if !found {
            return None;
        }
    }
}

/// Find the position of a byte subsequence in a buffer.
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Try to extract one complete SSE event from a string buffer.
fn try_extract_event(buffer: &mut String) -> Option<SseEvent> {
    loop {
        let mut found = false;
        for sep in ["\n\n", "\r\n\r\n"] {
            if let Some(pos) = buffer.find(sep) {
                let event_block = buffer[..pos].to_string();
                *buffer = buffer[pos + sep.len()..].to_string();

                if let Some(event) = parse_event_block(&event_block) {
                    return Some(event);
                }
                found = true;
                break; // Restart outer loop to check for more events
            }
        }
        if !found {
            return None;
        }
    }
}

/// Parse a single SSE event block into an SseEvent.
fn parse_event_block(block: &str) -> Option<SseEvent> {
    let mut event_type = None;
    let mut data_lines = Vec::new();

    for line in block.lines() {
        let line = line.trim_start_matches('\r');
        if let Some(val) = line.strip_prefix("event:") {
            event_type = Some(val.trim().to_string());
        } else if let Some(val) = line.strip_prefix("data:") {
            data_lines.push(val.trim_start().to_string());
        }
    }

    if data_lines.is_empty() {
        return None;
    }

    Some(SseEvent {
        event: event_type,
        data: data_lines.join("\n"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    fn make_stream(chunks: Vec<&str>) -> impl Stream<Item = String> + Send + 'static {
        let owned: Vec<String> = chunks.into_iter().map(|s| s.to_string()).collect();
        stream::iter(owned)
    }

    #[tokio::test]
    async fn test_single_event() {
        let events: Vec<SseEvent> = parse_sse_strings(make_stream(vec!["data: hello world\n\n"]))
            .collect()
            .await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello world");
        assert!(events[0].event.is_none());
    }

    #[tokio::test]
    async fn test_event_with_type() {
        let events: Vec<SseEvent> = parse_sse_strings(make_stream(vec![
            "event: message_start\ndata: {\"type\":\"start\"}\n\n",
        ]))
        .collect()
        .await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("message_start"));
        assert_eq!(events[0].data, "{\"type\":\"start\"}");
    }

    #[tokio::test]
    async fn test_multiple_events() {
        let events: Vec<SseEvent> =
            parse_sse_strings(make_stream(vec!["data: first\n\ndata: second\n\n"]))
                .collect()
                .await;
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data, "first");
        assert_eq!(events[1].data, "second");
    }

    #[tokio::test]
    async fn test_chunked_data() {
        let events: Vec<SseEvent> =
            parse_sse_strings(make_stream(vec!["data: hel", "lo\n\ndata: world\n\n"]))
                .collect()
                .await;
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data, "hello");
        assert_eq!(events[1].data, "world");
    }

    #[tokio::test]
    async fn test_done_sentinel() {
        let events: Vec<SseEvent> = parse_sse_strings(make_stream(vec!["data: [DONE]\n\n"]))
            .collect()
            .await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "[DONE]");
    }

    #[tokio::test]
    async fn test_comment_ignored() {
        let events: Vec<SseEvent> =
            parse_sse_strings(make_stream(vec![": comment\ndata: actual\n\n"]))
                .collect()
                .await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "actual");
    }

    /// Verify that multi-byte UTF-8 characters split across byte chunks
    /// are reassembled correctly and not replaced with U+FFFD.
    #[test]
    fn test_utf8_split_across_byte_chunks() {
        // "完成后" = E5AE8C E68890 E5908E
        // Split "成" (E6 88 90) across two chunks — this is exactly
        // the bug that caused "完��后" garbled text.
        let mut buffer = Vec::new();

        // Chunk 1: "data: " + "完" + first 2 bytes of "成"
        buffer.extend_from_slice(b"data: \xe5\xae\x8c\xe6\x88");
        assert!(try_extract_event_bytes(&mut buffer).is_none());

        // Chunk 2: last byte of "成" + "后" + event delimiter
        buffer.extend_from_slice(b"\x90\xe5\x90\x8e\n\n");
        let event = try_extract_event_bytes(&mut buffer).unwrap();

        assert!(
            !event.data.contains('\u{FFFD}'),
            "UTF-8 replacement character found: {}",
            event.data
        );
        assert_eq!(event.data, "完成后");
    }

    /// Verify CJK characters at chunk boundaries in multiple events.
    #[test]
    fn test_utf8_cjk_multiple_events_bytes() {
        let mut buffer = Vec::new();

        // "你好" = E4BDA0 E5A5BD
        // Two events, second split mid-character
        buffer.extend_from_slice(b"data: \xe4\xbd\xa0\n\ndata: \xe5\xa5");
        let event1 = try_extract_event_bytes(&mut buffer).unwrap();
        assert_eq!(event1.data, "你");

        // Second event still incomplete
        assert!(try_extract_event_bytes(&mut buffer).is_none());

        // Finish the second event
        buffer.extend_from_slice(b"\xbd\n\n");
        let event2 = try_extract_event_bytes(&mut buffer).unwrap();
        assert_eq!(event2.data, "好");
    }
}
