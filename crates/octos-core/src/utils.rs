//! Shared utility functions.

/// Truncate a string in-place at a UTF-8 safe boundary, appending a suffix.
///
/// Does nothing if `s.len() <= max_len`.
pub fn truncate_utf8(s: &mut String, max_len: usize, suffix: &str) {
    if s.len() <= max_len {
        return;
    }
    let mut limit = max_len;
    while limit > 0 && !s.is_char_boundary(limit) {
        limit -= 1;
    }
    s.truncate(limit);
    s.push_str(suffix);
}

/// Return a truncated copy of `s` at a UTF-8 safe boundary with suffix appended.
///
/// Returns the original string unchanged if `s.len() <= max_len`.
pub fn truncated_utf8(s: &str, max_len: usize, suffix: &str) -> String {
    if s.len() <= max_len {
        return s.to_string();
    }
    let mut limit = max_len;
    while limit > 0 && !s.is_char_boundary(limit) {
        limit -= 1;
    }
    format!("{}{}", &s[..limit], suffix)
}

/// Truncate output with head/tail split, preserving both ends.
///
/// When `s` exceeds `max_len`, keeps `head_ratio` fraction from the start and
/// the remainder from the end, joined by a separator line showing omitted bytes.
/// Both split points are UTF-8 safe.
pub fn truncate_head_tail(s: &str, max_len: usize, head_ratio: f32) -> String {
    if s.len() <= max_len {
        return s.to_string();
    }

    let head_ratio = head_ratio.clamp(0.1, 0.9);

    // Estimate separator overhead conservatively (handles large omitted counts)
    // "\n\n... [99999 bytes omitted] ...\n\n" is ~40 bytes max
    let sep_overhead = 50;
    let available = max_len.saturating_sub(sep_overhead);
    let head_budget = (available as f32 * head_ratio) as usize;
    let tail_budget = available.saturating_sub(head_budget);

    // Find UTF-8 safe boundaries
    let mut head_end = head_budget.min(s.len());
    while head_end > 0 && !s.is_char_boundary(head_end) {
        head_end -= 1;
    }

    let mut tail_start = s.len().saturating_sub(tail_budget);
    while tail_start < s.len() && !s.is_char_boundary(tail_start) {
        tail_start += 1;
    }

    // Avoid overlap
    if head_end >= tail_start {
        return s.to_string();
    }

    let omitted = tail_start - head_end;
    let sep = format!("\n\n... [{omitted} bytes omitted] ...\n\n");
    format!("{}{}{}", &s[..head_end], sep, &s[tail_start..])
}

/// Default per-tool output limits (max chars). Tools not listed use the global default.
pub fn tool_output_limit(tool_name: &str) -> usize {
    match tool_name {
        "read_file" => 50_000,
        "shell" => 30_000,
        "grep" => 30_000,
        "web_fetch" => 40_000,
        "web_search" => 20_000,
        "deep_search" => 50_000,
        "deep_research" => 50_000,
        "spawn" => 50_000,
        _ => 50_000, // global default
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_no_op() {
        let mut s = "hello".to_string();
        truncate_utf8(&mut s, 10, "...");
        assert_eq!(s, "hello");
    }

    #[test]
    fn test_truncate_ascii() {
        let mut s = "abcdefghij".to_string();
        truncate_utf8(&mut s, 5, "...");
        assert_eq!(s, "abcde...");
    }

    #[test]
    fn test_truncate_utf8_boundary() {
        // 你好世 = 9 bytes, truncate at 7 should back up to byte 6
        let mut s = "\u{4F60}\u{597D}\u{4E16}".to_string();
        truncate_utf8(&mut s, 7, "...");
        assert_eq!(s, "\u{4F60}\u{597D}...");
    }

    #[test]
    fn test_truncated_utf8_no_op() {
        assert_eq!(truncated_utf8("hello", 10, "..."), "hello");
    }

    #[test]
    fn test_truncated_utf8_ascii() {
        assert_eq!(truncated_utf8("abcdefghij", 5, "..."), "abcde...");
    }

    #[test]
    fn test_truncated_utf8_boundary() {
        let s = "\u{4F60}\u{597D}\u{4E16}"; // 9 bytes
        assert_eq!(truncated_utf8(s, 7, "..."), "\u{4F60}\u{597D}...");
    }

    #[test]
    fn test_head_tail_no_op() {
        let s = "short text";
        assert_eq!(truncate_head_tail(s, 100, 0.5), "short text");
    }

    #[test]
    fn test_head_tail_split() {
        // 100 chars of 'a', 100 chars of 'b'
        let s = format!("{}{}", "a".repeat(100), "b".repeat(100));
        let result = truncate_head_tail(&s, 100, 0.5);
        assert!(result.starts_with("aaa"));
        assert!(result.ends_with("bbb"));
        assert!(result.contains("bytes omitted"));
        assert!(result.len() <= 150); // 100 + separator overhead
    }

    #[test]
    fn test_head_tail_preserves_utf8() {
        let s = format!("{}{}", "\u{4F60}".repeat(50), "\u{597D}".repeat(50));
        let result = truncate_head_tail(&s, 100, 0.5);
        // Should not panic or produce invalid UTF-8
        assert!(result.is_char_boundary(0));
        assert!(result.contains("bytes omitted"));
    }

    #[test]
    fn test_tool_output_limit() {
        assert_eq!(tool_output_limit("read_file"), 50_000);
        assert_eq!(tool_output_limit("shell"), 30_000);
        assert_eq!(tool_output_limit("unknown_tool"), 50_000);
    }
}
