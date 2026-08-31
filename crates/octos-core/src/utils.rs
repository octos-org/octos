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

/// Which limit cut the output in a [`TruncationReport`].
///
/// Modeled on pi's `TruncationResult.truncatedBy` (`"lines" | "bytes" |
/// null`, `packages/coding-agent/src/core/tools/truncate.ts`).
/// [`truncate_head_tail_report`] cuts purely by bytes, so today it only ever
/// emits [`TruncatedBy::Bytes`]; [`TruncatedBy::Lines`] is declared for
/// future line-count-based truncation helpers rather than invented here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncatedBy {
    /// The byte limit (`max_len`) was hit.
    Bytes,
    /// A line-count limit was hit. Not produced by any octos-core helper
    /// yet — see the enum-level doc.
    Lines,
}

/// Structured result of a head/tail truncation, modeled on pi's
/// `TruncationResult` (`packages/coding-agent/src/core/tools/truncate.ts`).
///
/// [`truncate_head_tail`] keeps only [`TruncationReport::content`] and throws
/// the rest away; callers that need to ADVISE the model about the cut (how
/// much is gone, which limit fired) use [`truncate_head_tail_report`] and
/// read it from here instead of re-deriving it from string lengths — the
/// re-derivation `total - content.len()` undercounts by the elision marker's
/// own length.
#[derive(Debug, Clone, PartialEq)]
pub struct TruncationReport {
    /// The (possibly truncated) output. When truncated this embeds the
    /// `\n\n... [N bytes omitted] ...\n\n` elision marker between the kept
    /// head and tail.
    pub content: String,
    /// Whether any input bytes were dropped.
    pub truncated: bool,
    /// Which limit fired; `None` when not truncated. Only
    /// [`TruncatedBy::Bytes`] is reachable from
    /// [`truncate_head_tail_report`].
    pub truncated_by: Option<TruncatedBy>,
    /// Byte length of the original input.
    pub total_bytes: usize,
    /// Byte length of `content`. When truncated this INCLUDES the elision
    /// marker, so `output_bytes != total_bytes - omitted_bytes` — the marker
    /// adds ~30 bytes. In the degenerate `max_len`-below-marker-overhead
    /// regime it can even exceed `max_len` (pre-existing
    /// [`truncate_head_tail`] behaviour, unchanged).
    pub output_bytes: usize,
    /// Exact number of input bytes dropped between head and tail — the same
    /// `N` printed in the elision marker. `0` when not truncated.
    pub omitted_bytes: usize,
    /// pi's `firstLineExceedsLimit` flags the head-truncation case where not
    /// even the FIRST LINE fits the byte budget. A head/tail byte split has
    /// no lines, so the honest analogue reported here: `true` when
    /// truncation kept ZERO head bytes — `max_len` (minus the marker
    /// overhead reservation) was too small for even one leading char to
    /// survive, and `content` is effectively just the marker.
    pub first_segment_exceeds_limit: bool,
    /// The applied byte limit, as passed.
    pub max_len: usize,
    /// The applied head fraction, after clamping to `[0.1, 0.9]`.
    pub head_ratio: f32,
}

/// Truncate output with head/tail split, preserving both ends.
///
/// When `s` exceeds `max_len`, keeps `head_ratio` fraction from the start and
/// the remainder from the end, joined by a separator line showing omitted bytes.
/// Both split points are UTF-8 safe.
///
/// Thin wrapper over [`truncate_head_tail_report`] keeping the legacy
/// `String` surface — one implementation, two surfaces.
pub fn truncate_head_tail(s: &str, max_len: usize, head_ratio: f32) -> String {
    truncate_head_tail_report(s, max_len, head_ratio).content
}

/// [`truncate_head_tail`] returning a structured [`TruncationReport`]
/// instead of a bare `String`.
///
/// The `content` field is byte-identical to what [`truncate_head_tail`]
/// returns for the same inputs; the rest of the report tells the caller what
/// the cut did (`omitted_bytes`, `truncated_by`, the applied limits) without
/// re-deriving it from string lengths.
pub fn truncate_head_tail_report(s: &str, max_len: usize, head_ratio: f32) -> TruncationReport {
    let total_bytes = s.len();
    let head_ratio = head_ratio.clamp(0.1, 0.9);

    let untruncated = |content: String| TruncationReport {
        content,
        truncated: false,
        truncated_by: None,
        total_bytes,
        output_bytes: total_bytes,
        omitted_bytes: 0,
        first_segment_exceeds_limit: false,
        max_len,
        head_ratio,
    };

    if s.len() <= max_len {
        return untruncated(s.to_string());
    }

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

    // Defensive overlap guard: the budgets cannot overlap by construction
    // (head + tail <= max_len - 50 < s.len() here), but if they ever did the
    // input passes through whole — reported honestly as "not truncated".
    if head_end >= tail_start {
        return untruncated(s.to_string());
    }

    let omitted = tail_start - head_end;
    let sep = format!("\n\n... [{omitted} bytes omitted] ...\n\n");
    let content = format!("{}{}{}", &s[..head_end], sep, &s[tail_start..]);
    TruncationReport {
        output_bytes: content.len(),
        content,
        truncated: true,
        truncated_by: Some(TruncatedBy::Bytes),
        total_bytes,
        omitted_bytes: omitted,
        first_segment_exceeds_limit: head_end == 0,
        max_len,
        head_ratio,
    }
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

/// Maximum byte length of a [`safe_filename`] result.
pub const SAFE_FILENAME_MAX_BYTES: usize = 80;
/// Byte budget for the encoded stem before the hash suffix kicks in.
const SAFE_FILENAME_STEM_BYTES: usize = 64;

/// Turn an arbitrary string into a filesystem-safe filename stem.
///
/// - ASCII alphanumerics, `-` and `_` pass through; every other byte is
///   percent-encoded (`%XX` of its UTF-8 bytes), which makes the mapping
///   injective for un-clamped names — distinct inputs can never collide.
/// - Results longer than [`SAFE_FILENAME_STEM_BYTES`] are clamped (never
///   splitting a `%XX` triplet) and suffixed with `-<8 hex>` of the
///   original name's SHA-256, so clamped names stay collision-resistant.
/// - The result never exceeds [`SAFE_FILENAME_MAX_BYTES`] bytes, contains
///   no path separators or dots, and is never empty.
///
/// Shared naming helper for memory artifacts (staging notes/extractions,
/// backups); callers append their own extension.
pub fn safe_filename(name: &str) -> String {
    let mut encoded = String::with_capacity(name.len());
    for b in name.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' => encoded.push(b as char),
            _ => encoded.push_str(&format!("%{b:02X}")),
        }
    }

    if encoded.is_empty() {
        encoded.push('_');
    }
    if encoded.len() <= SAFE_FILENAME_STEM_BYTES {
        return encoded;
    }

    // Clamp without splitting a %XX triplet: back up while the cut point
    // lands inside one (a '%' at cut-1 or cut-2 started a triplet that
    // extends past the cut).
    let mut cut = SAFE_FILENAME_STEM_BYTES;
    let bytes = encoded.as_bytes();
    while cut > 0 && ((cut >= 1 && bytes[cut - 1] == b'%') || (cut >= 2 && bytes[cut - 2] == b'%'))
    {
        cut -= 1;
    }
    encoded.truncate(cut);

    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(name.as_bytes());
    let mut suffix = String::with_capacity(9);
    suffix.push('-');
    for byte in digest.iter().take(4) {
        suffix.push_str(&format!("{byte:02x}"));
    }
    encoded.push_str(&suffix);
    encoded
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

    /// Characterization pin: the exact legacy string emitted by
    /// `truncate_head_tail`, captured BEFORE the structured-report refactor.
    /// The refactor must keep the wrapper byte-identical.
    #[test]
    fn should_emit_exact_legacy_marker_when_truncating() {
        let s = format!("{}{}", "h".repeat(100), "t".repeat(100));
        let expect = format!(
            "{}\n\n... [150 bytes omitted] ...\n\n{}",
            "h".repeat(25),
            "t".repeat(25)
        );
        assert_eq!(truncate_head_tail(&s, 100, 0.5), expect);
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

    #[test]
    fn should_pass_through_plain_ascii_when_safe_filename() {
        assert_eq!(safe_filename("hello-world_1"), "hello-world_1");
    }

    #[test]
    fn should_percent_encode_specials_and_cjk_when_safe_filename() {
        assert_eq!(safe_filename("a b"), "a%20b");
        assert_eq!(safe_filename("a.b/c"), "a%2Eb%2Fc");
        // "密" = E5 AF 86 in UTF-8
        assert_eq!(safe_filename("密"), "%E5%AF%86");
    }

    #[test]
    fn should_stay_injective_when_names_differ_only_by_special_chars() {
        // The legacy char-replace slugging collapsed these; percent-encoding must not.
        assert_ne!(safe_filename("a/b"), safe_filename("a_b"));
        assert_ne!(safe_filename("a b"), safe_filename("a-b"));
    }

    #[test]
    fn should_contain_no_path_or_dot_bytes_when_input_is_hostile() {
        let out = safe_filename("../../etc/passwd\0~");
        assert!(!out.contains('/'));
        assert!(!out.contains('\\'));
        assert!(!out.contains('.'));
        assert!(!out.contains('\0'));
        assert!(!out.contains('~'));
    }

    #[test]
    fn should_clamp_with_distinct_hash_suffix_when_names_share_long_prefix() {
        let prefix = "x".repeat(100);
        let a = safe_filename(&format!("{prefix}-alpha"));
        let b = safe_filename(&format!("{prefix}-beta"));
        assert!(a.len() <= SAFE_FILENAME_MAX_BYTES);
        assert!(b.len() <= SAFE_FILENAME_MAX_BYTES);
        assert_ne!(a, b, "hash suffix must disambiguate clamped names");
    }

    #[test]
    fn should_not_split_percent_triplet_when_clamping() {
        // All-CJK input: every char encodes to three %XX triplets (9 bytes),
        // so a naive 64-byte cut would land mid-triplet.
        let name = "记".repeat(40);
        let out = safe_filename(&name);
        assert!(out.len() <= SAFE_FILENAME_MAX_BYTES);
        // Every '%' must be followed by two hex digits within the stem.
        let stem = &out[..out.rfind('-').unwrap()];
        let bytes = stem.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' {
                assert!(
                    i + 2 < bytes.len()
                        && bytes[i + 1].is_ascii_hexdigit()
                        && bytes[i + 2].is_ascii_hexdigit(),
                    "dangling percent triplet in {out}"
                );
                i += 3;
            } else {
                i += 1;
            }
        }
    }

    #[test]
    fn should_return_placeholder_when_input_empty() {
        assert_eq!(safe_filename(""), "_");
    }

    // ── structured truncation report (pi TruncationResult port) ──────────

    #[test]
    fn should_report_untruncated_when_input_at_exact_limit() {
        let s = "a".repeat(100);
        let r = truncate_head_tail_report(&s, 100, 0.7);
        assert!(!r.truncated);
        assert_eq!(r.truncated_by, None);
        assert_eq!(r.content, s);
        assert_eq!(r.total_bytes, 100);
        assert_eq!(r.output_bytes, 100);
        assert_eq!(r.omitted_bytes, 0);
        assert!(!r.first_segment_exceeds_limit);
        assert_eq!(r.max_len, 100);
    }

    #[test]
    fn should_report_bytes_truncation_when_one_byte_over_limit() {
        let s = "a".repeat(101);
        let r = truncate_head_tail_report(&s, 100, 0.7);
        assert!(r.truncated);
        assert_eq!(r.truncated_by, Some(TruncatedBy::Bytes));
        assert_eq!(r.total_bytes, 101);
        assert_eq!(r.max_len, 100);
        assert_eq!(r.output_bytes, r.content.len());
        assert!(r.omitted_bytes > 0);
        assert!(
            r.content
                .contains(&format!("... [{} bytes omitted] ...", r.omitted_bytes)),
            "report and inline marker must agree on the omitted count: {}",
            r.content
        );
        assert!(!r.first_segment_exceeds_limit);
    }

    #[test]
    fn should_report_untruncated_when_input_empty() {
        let r = truncate_head_tail_report("", 0, 0.7);
        assert!(!r.truncated);
        assert_eq!(r.truncated_by, None);
        assert_eq!(r.content, "");
        assert_eq!(r.total_bytes, 0);
        assert_eq!(r.output_bytes, 0);
        assert_eq!(r.omitted_bytes, 0);
    }

    #[test]
    fn should_keep_utf8_boundaries_when_multibyte_chars_straddle_the_cut() {
        // 4-byte scalars; the byte budgets land mid-char, so both split points
        // must back off to char boundaries instead of panicking.
        let s = "\u{1F980}".repeat(200); // 800 bytes
        let r = truncate_head_tail_report(&s, 101, 0.7);
        assert!(r.truncated);
        let marker = format!("\n\n... [{} bytes omitted] ...\n\n", r.omitted_bytes);
        let (head, tail) = r
            .content
            .split_once(&marker)
            .expect("truncated content must contain the elision marker");
        assert!(
            head.chars().all(|c| c == '\u{1F980}'),
            "head must hold only whole chars: {head:?}"
        );
        assert!(
            tail.chars().all(|c| c == '\u{1F980}'),
            "tail must hold only whole chars: {tail:?}"
        );
        assert_eq!(r.total_bytes, 800);
        assert_eq!(r.output_bytes, r.content.len());
        // Whole chars only: kept payload + omitted covers the input exactly.
        assert_eq!(head.len() + tail.len() + r.omitted_bytes, r.total_bytes);
    }

    /// Wrapper equivalence: `truncate_head_tail` must be a thin projection of
    /// the report — one implementation, two surfaces. Property-style over
    /// fixtures crossing the limit from both sides, multi-byte content, and
    /// out-of-range ratios.
    #[test]
    fn should_match_wrapper_content_when_report_and_wrapper_share_inputs() {
        let fixtures: Vec<String> = vec![
            String::new(),
            "short".to_string(),
            "a".repeat(99),
            "a".repeat(100),
            "a".repeat(101),
            "x".repeat(10_000),
            "\u{1F980}".repeat(400),
            "\u{4F60}\u{597D}\u{4E16}\u{754C}".repeat(500),
            format!("head\n{}\ntail", "mid ".repeat(2_000)),
        ];
        for s in &fixtures {
            for max_len in [0usize, 10, 49, 50, 51, 100, 1_000, 30_000] {
                for ratio in [0.0f32, 0.3, 0.5, 0.7, 0.9, 1.5] {
                    let report = truncate_head_tail_report(s, max_len, ratio);
                    assert_eq!(
                        truncate_head_tail(s, max_len, ratio),
                        report.content,
                        "wrapper and report must share one implementation \
                         (len={}, max_len={max_len}, ratio={ratio})",
                        s.len(),
                    );
                }
            }
        }
    }

    #[test]
    fn should_flag_first_segment_exceeds_limit_when_budget_below_marker_overhead() {
        // max_len below the separator overhead: no payload byte survives and
        // the emitted content is only the elision marker — the head/tail
        // analogue of pi's firstLineExceedsLimit (see the field doc).
        let s = "z".repeat(100);
        let r = truncate_head_tail_report(&s, 10, 0.7);
        assert!(r.truncated);
        assert!(r.first_segment_exceeds_limit);
        assert_eq!(r.omitted_bytes, 100);
        assert_eq!(r.content, "\n\n... [100 bytes omitted] ...\n\n");
        assert_eq!(r.truncated_by, Some(TruncatedBy::Bytes));
    }

    #[test]
    fn should_clamp_reported_head_ratio_when_ratio_out_of_range() {
        let s = "a".repeat(300);
        let hi = truncate_head_tail_report(&s, 100, 5.0);
        assert_eq!(hi.head_ratio, 0.9);
        assert_eq!(hi.content, truncate_head_tail(&s, 100, 5.0));
        let lo = truncate_head_tail_report(&s, 100, -1.0);
        assert_eq!(lo.head_ratio, 0.1);
    }
}
