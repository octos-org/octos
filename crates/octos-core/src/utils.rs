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
///
/// High-volume aggregation tools (`news_fetch`, `search` / `deep_search`)
/// intentionally exceed the 50K default: their JSON payloads bundle dozens of
/// headlines or hits in a single call. When their output is middle-elided the
/// LLM mistakes the elision marker for "incomplete results" and retries with
/// drifting arguments — see the `web-1779494658716-mxrxe8` diagnostic and PR
/// `fix/news-fetch-loop-and-detect-recovery`.
///
/// Note on `search` vs `deep_search`: the bundled deep-search skill exposes
/// its runtime tool as `search` (see `app-skills/deep-search/manifest.json`
/// — `"tool_name": "search"`). Execution looks limits up by the runtime tool
/// name, so the 200K budget MUST be keyed on `search` to take effect for the
/// shipping skill. `deep_search` is kept as a defensive alias / contract slot
/// for future variants and any external consumers that key on the contract
/// name rather than the runtime name.
pub fn tool_output_limit(tool_name: &str) -> usize {
    match tool_name {
        "read_file" => 50_000,
        "shell" => 30_000,
        "grep" => 30_000,
        "web_fetch" => 40_000,
        "web_search" => 20_000,
        // `search` is the runtime tool name of the bundled deep-search skill
        // (see `app-skills/deep-search/manifest.json`); `deep_search` is the
        // contract slot kept as a defensive alias for future variants.
        "search" => 200_000,
        "deep_search" => 200_000,
        "deep_research" => 50_000,
        "news_fetch" => 200_000,
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

    /// Regression: `news_fetch` returns a JSON payload bundling dozens of
    /// headlines and can easily exceed the 50K global default. When the
    /// output is middle-elided ("... [N bytes omitted] ..."), kimi-class
    /// models mistake the marker for incomplete results and retry with
    /// drifting `categories=` argument lists — the exact spiral observed
    /// on session `web-1779494658716-mxrxe8` (ledger seq 214-562). Guard
    /// against a future silent shrink.
    #[test]
    fn news_fetch_limit_is_at_least_100k_bytes() {
        assert!(
            tool_output_limit("news_fetch") >= 100_000,
            "news_fetch tool_output_limit must stay >=100K bytes to avoid \
             middle-elision triggering a retry spiral; current value is {}",
            tool_output_limit("news_fetch")
        );
    }

    /// Companion regression for `deep_search` AND the runtime tool name
    /// `search` exposed by the bundled deep-search skill
    /// (`app-skills/deep-search/manifest.json` — `"tool_name": "search"`).
    ///
    /// Execution keys the truncation budget on the runtime tool name, so
    /// the `deep_search` arm alone never takes effect for the shipping skill.
    /// We MUST guard both — `search` is the load-bearing one in production,
    /// `deep_search` is the contract-slot alias for future variants and any
    /// external consumers that key on the contract name.
    #[test]
    fn deep_search_limit_is_at_least_100k_bytes() {
        assert!(
            tool_output_limit("search") >= 100_000,
            "search tool_output_limit must stay >=100K bytes — this is the \
             runtime tool name of the bundled deep-search skill, and elision \
             of its aggregated payload causes the same retry spiral as \
             news_fetch; current value is {}",
            tool_output_limit("search")
        );
        assert!(
            tool_output_limit("deep_search") >= 100_000,
            "deep_search tool_output_limit must stay >=100K bytes to avoid \
             middle-elision triggering retry behaviour; current value is {}",
            tool_output_limit("deep_search")
        );
    }
}
