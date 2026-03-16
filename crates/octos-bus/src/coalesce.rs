//! Message coalescing: split long messages into channel-safe chunks.

use tracing::warn;

/// Configuration for message splitting.
pub struct ChunkConfig {
    /// Maximum characters per chunk (platform limit).
    pub max_chars: usize,
}

impl ChunkConfig {
    pub fn telegram() -> Self {
        Self { max_chars: 4000 }
    }
    pub fn discord() -> Self {
        Self { max_chars: 1900 }
    }
    pub fn slack() -> Self {
        Self { max_chars: 3900 }
    }
    pub fn default_limit() -> Self {
        Self { max_chars: 4000 }
    }
}

/// Maximum number of chunks to prevent DoS from massive messages.
const MAX_CHUNKS: usize = 50;

/// Split text into channel-safe chunks.
///
/// Prefers breaking at paragraph boundaries (`\n\n`), then newlines (`\n`),
/// then sentence endings (`. `), then spaces, and finally hard-cuts as a last resort.
/// Produces at most [`MAX_CHUNKS`] chunks; remaining text is truncated with a marker.
pub fn split_message(text: &str, config: &ChunkConfig) -> Vec<String> {
    if text.len() <= config.max_chars {
        return if text.is_empty() {
            vec![]
        } else {
            vec![text.to_string()]
        };
    }

    let mut chunks = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        if chunks.len() >= MAX_CHUNKS {
            warn!(
                original_chars = text.len(),
                remaining_chars = remaining.len(),
                "message exceeded {MAX_CHUNKS} chunks, truncating",
            );
            chunks.push(format!(
                "[message truncated - {} chars omitted]",
                remaining.len()
            ));
            break;
        }

        if remaining.len() <= config.max_chars {
            chunks.push(remaining.to_string());
            break;
        }

        // Find the last valid char boundary at or before max_chars
        let mut limit = config.max_chars.min(remaining.len());
        while limit > 0 && !remaining.is_char_boundary(limit) {
            limit -= 1;
        }
        let search = &remaining[..limit];
        let break_at = find_break_point(search);

        chunks.push(remaining[..break_at].trim_end().to_string());
        remaining = remaining[break_at..].trim_start_matches('\n');
        // Skip a single leading space after break (not in middle of word)
        if remaining.starts_with(' ') && !remaining.starts_with("  ") {
            remaining = &remaining[1..];
        }
    }

    chunks
}

/// Find the best break point within `text`, preferring natural boundaries.
///
/// Rejects break points at position 0 to guarantee forward progress.
fn find_break_point(text: &str) -> usize {
    // Try paragraph break
    if let Some(pos) = text.rfind("\n\n") {
        if pos > 0 {
            return pos;
        }
    }
    // Try newline
    if let Some(pos) = text.rfind('\n') {
        if pos > 0 {
            return pos;
        }
    }
    // Try sentence end
    if let Some(pos) = text.rfind(". ") {
        if pos > 0 {
            return pos + 1;
        } // include the period
    }
    // Try space
    if let Some(pos) = text.rfind(' ') {
        if pos > 0 {
            return pos;
        }
    }
    // Hard cut at char boundary
    let mut end = text.len();
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    // Safety: if somehow end == 0 (shouldn't happen since text.len() > 0 and
    // we're called with text = remaining[..max_chars]), fall back to full text.
    if end == 0 { text.len() } else { end }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_split_needed() {
        let config = ChunkConfig { max_chars: 100 };
        let chunks = split_message("Hello world", &config);
        assert_eq!(chunks, vec!["Hello world"]);
    }

    #[test]
    fn test_empty_input() {
        let config = ChunkConfig { max_chars: 100 };
        let chunks = split_message("", &config);
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_paragraph_split() {
        let config = ChunkConfig { max_chars: 30 };
        let text = "First paragraph.\n\nSecond paragraph here.";
        let chunks = split_message(text, &config);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], "First paragraph.");
        assert_eq!(chunks[1], "Second paragraph here.");
    }

    #[test]
    fn test_newline_split() {
        let config = ChunkConfig { max_chars: 20 };
        let text = "Line one here\nLine two here";
        let chunks = split_message(text, &config);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], "Line one here");
        assert_eq!(chunks[1], "Line two here");
    }

    #[test]
    fn test_sentence_split() {
        let config = ChunkConfig { max_chars: 25 };
        let text = "First sentence. Second sentence here.";
        let chunks = split_message(text, &config);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], "First sentence.");
        assert_eq!(chunks[1], "Second sentence here.");
    }

    #[test]
    fn test_hard_cut() {
        let config = ChunkConfig { max_chars: 10 };
        let text = "abcdefghijklmnopqrst";
        let chunks = split_message(text, &config);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], "abcdefghij");
        assert_eq!(chunks[1], "klmnopqrst");
    }

    #[test]
    fn test_hard_cut_unicode() {
        // Each CJK char is 3 bytes in UTF-8. With max_chars=7, a naive byte
        // slice at 7 would land in the middle of the 3rd char (bytes 6-8).
        // The hard-cut must back up to the char boundary at 6.
        let config = ChunkConfig { max_chars: 7 };
        let text = "abcdefghi";
        let chunks = split_message(text, &config);
        assert_eq!(chunks[0], "abcdefg");

        // Multi-byte: 3 CJK chars = 9 bytes, max_chars=8 should split at byte 6
        let config2 = ChunkConfig { max_chars: 8 };
        let text2 = "\u{4F60}\u{597D}\u{4E16}"; // 你好世 (9 bytes)
        let chunks2 = split_message(text2, &config2);
        assert_eq!(chunks2.len(), 2);
        assert_eq!(chunks2[0], "\u{4F60}\u{597D}"); // 你好
        assert_eq!(chunks2[1], "\u{4E16}"); // 世
    }

    #[test]
    fn test_truncation_on_massive_input() {
        let config = ChunkConfig { max_chars: 10 };
        // 51 chunks worth of content → last chunk should be truncation marker
        let text = "abcdefghij".repeat(100); // 1000 chars, 100 chunks at max_chars=10
        let chunks = split_message(&text, &config);
        assert_eq!(chunks.len(), 51); // 50 real + 1 truncation marker
        assert!(chunks[50].starts_with("[message truncated"));
    }
}
