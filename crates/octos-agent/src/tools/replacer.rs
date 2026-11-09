//! Cascading fuzzy replacer chain for `edit_file` (#1771).
//!
//! LLMs routinely produce `old_string` values with minor whitespace,
//! indentation, or escape differences. Instead of failing hard on anything
//! that is not an exact match, `edit_file` runs this chain of increasingly
//! tolerant matchers **in order** and uses the first one that finds any
//! match:
//!
//! 1. `exact` — plain substring match (today's behaviour, always first)
//! 2. `line_trimmed` — per-line trim, line-window equality
//! 3. `whitespace_normalized` — collapse whitespace runs to single spaces
//! 4. `indentation_flexible` — strip blank boundary lines and the common
//!    minimum indentation from both sides, then compare exactly
//! 5. `escape_normalized` — unescape literal `\n` / `\t` / `\\` etc. in the
//!    needle before searching
//! 6. `block_anchor` — anchor on the first + last trimmed lines, score
//!    candidate spans by average middle-line Levenshtein similarity with
//!    unpaired lines counting against the score, accept at >= 0.65
//!
//! Uniqueness is still enforced: the first stage that matches at all decides
//! the outcome — exactly one location wins, more than one is an ambiguity
//! error carrying the count. Later (fuzzier) stages never get to overrule an
//! ambiguous earlier stage. The [`is_disproportionate_match`] guard rejects
//! runaway fuzzy spans after the fact.

use std::ops::Range;

/// Minimum block similarity (see [`block_similarity`]) for the
/// `block_anchor` replacer to accept a candidate block.
const BLOCK_ANCHOR_SIMILARITY_THRESHOLD: f64 = 0.65;

/// Outcome of running the replacer chain over a file's content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChainOutcome {
    /// No stage produced any match.
    NoMatch,
    /// The first stage that matched found more than one location.
    Ambiguous {
        count: usize,
        replacer: &'static str,
    },
    /// The first stage that matched found exactly one location.
    Match {
        range: Range<usize>,
        replacer: &'static str,
    },
}

/// Byte span of one line's content (newline excluded).
#[derive(Debug, Clone, Copy)]
struct LineSpan {
    start: usize,
    end: usize,
}

impl LineSpan {
    fn text<'a>(&self, content: &'a str) -> &'a str {
        &content[self.start..self.end]
    }
}

/// Run the cascading replacer chain. The first stage with any matches
/// decides the outcome (see module docs).
pub(crate) fn find_replacement(content: &str, find: &str) -> ChainOutcome {
    if find.is_empty() {
        return ChainOutcome::NoMatch;
    }
    let lines = index_lines(content);
    for stage in 0..6u8 {
        let (name, matches): (&'static str, Vec<Range<usize>>) = match stage {
            0 => ("exact", exact_matches(content, find)),
            1 => ("line_trimmed", line_trimmed_matches(content, &lines, find)),
            2 => (
                "whitespace_normalized",
                whitespace_normalized_matches(content, &lines, find),
            ),
            3 => (
                "indentation_flexible",
                indentation_flexible_matches(content, &lines, find),
            ),
            4 => (
                "escape_normalized",
                escape_normalized_matches(content, find),
            ),
            5 => ("block_anchor", block_anchor_matches(content, &lines, find)),
            _ => unreachable!(),
        };
        match matches.len() {
            0 => continue,
            1 => {
                return ChainOutcome::Match {
                    range: matches.into_iter().next().expect("len checked"),
                    replacer: name,
                };
            }
            count => {
                return ChainOutcome::Ambiguous {
                    count,
                    replacer: name,
                };
            }
        }
    }
    ChainOutcome::NoMatch
}

/// Safety guard: reject fuzzy matches whose span is far larger than the
/// `old_string` that produced them — a runaway match would silently destroy
/// unrelated code. Rejects when the span exceeds
/// `max(old_lines + 3, old_lines * 2)` lines or 4x the byte count.
pub(crate) fn is_disproportionate_match(matched: &str, find: &str) -> bool {
    let matched_lines = matched.lines().count();
    let find_lines = find.lines().count().max(1);
    let line_cap = (find_lines + 3).max(find_lines * 2);
    matched_lines > line_cap || matched.len() > find.len().saturating_mul(4)
}

// ---------------------------------------------------------------------------
// Stage 1: exact
// ---------------------------------------------------------------------------

fn exact_matches(content: &str, find: &str) -> Vec<Range<usize>> {
    content
        .match_indices(find)
        .map(|(i, m)| i..i + m.len())
        .collect()
}

// ---------------------------------------------------------------------------
// Line indexing shared by the window-based stages
// ---------------------------------------------------------------------------

fn index_lines(content: &str) -> Vec<LineSpan> {
    let mut spans = Vec::new();
    let mut start = 0usize;
    for segment in content.split('\n') {
        let end = start + segment.len();
        spans.push(LineSpan { start, end });
        start = end + 1; // skip the '\n'
    }
    spans
}

/// Split the needle into lines. A single trailing newline is popped off (it
/// produces a phantom empty line) and remembered so the matched span can
/// consume the file's newline symmetrically.
fn split_find(find: &str) -> (Vec<&str>, bool) {
    let trailing_newline = find.ends_with('\n');
    let mut lines: Vec<&str> = find.split('\n').collect();
    if trailing_newline {
        lines.pop();
    }
    (lines, trailing_newline)
}

/// Byte range of the window `[i, i + n)` of content lines. When the needle
/// carried a trailing newline, extend over the file's newline too so a
/// `new_string` that also ends in `\n` doesn't leave a doubled blank line.
fn window_range(
    content: &str,
    lines: &[LineSpan],
    i: usize,
    n: usize,
    consume_trailing_newline: bool,
) -> Range<usize> {
    let start = lines[i].start;
    let mut end = lines[i + n - 1].end;
    if consume_trailing_newline && content.as_bytes().get(end) == Some(&b'\n') {
        end += 1;
    }
    start..end
}

/// Generic line-window scan: yield every window of `find_lines.len()`
/// consecutive content lines for which `eq(window_line, find_line)` holds
/// pairwise.
fn window_scan(
    content: &str,
    lines: &[LineSpan],
    find_lines: &[&str],
    trailing_newline: bool,
    eq: impl Fn(&str, &str) -> bool,
) -> Vec<Range<usize>> {
    let n = find_lines.len();
    let mut out = Vec::new();
    if n == 0 || lines.len() < n {
        return out;
    }
    for i in 0..=(lines.len() - n) {
        let all_equal = find_lines
            .iter()
            .enumerate()
            .all(|(j, fl)| eq(lines[i + j].text(content), fl));
        if all_equal {
            out.push(window_range(content, lines, i, n, trailing_newline));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Stage 2: line_trimmed
// ---------------------------------------------------------------------------

fn line_trimmed_matches(content: &str, lines: &[LineSpan], find: &str) -> Vec<Range<usize>> {
    let (find_lines, trailing_newline) = split_find(find);
    window_scan(content, lines, &find_lines, trailing_newline, |a, b| {
        a.trim() == b.trim()
    })
}

// ---------------------------------------------------------------------------
// Stage 3: whitespace_normalized
// ---------------------------------------------------------------------------

/// Collapse every whitespace run to a single space and trim the ends.
fn normalize_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn whitespace_normalized_matches(
    content: &str,
    lines: &[LineSpan],
    find: &str,
) -> Vec<Range<usize>> {
    let (find_lines, trailing_newline) = split_find(find);
    window_scan(content, lines, &find_lines, trailing_newline, |a, b| {
        normalize_whitespace(a) == normalize_whitespace(b)
    })
}

// ---------------------------------------------------------------------------
// Stage 4: indentation_flexible
// ---------------------------------------------------------------------------

/// Byte offset of the first non-whitespace character (== line length for
/// blank lines).
fn leading_ws_len(s: &str) -> usize {
    s.char_indices()
        .find(|(_, c)| !c.is_whitespace())
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

/// Strip up to `upto` bytes of leading whitespace, never splitting a char.
fn strip_indent(s: &str, upto: usize) -> &str {
    let mut cut = 0usize;
    for (i, c) in s.char_indices() {
        if !c.is_whitespace() || i + c.len_utf8() > upto {
            break;
        }
        cut = i + c.len_utf8();
    }
    &s[cut..]
}

/// Common minimum indentation (in bytes) over non-blank lines.
fn common_indent<'a>(lines: impl Iterator<Item = &'a str>) -> usize {
    lines
        .filter(|l| !l.trim().is_empty())
        .map(leading_ws_len)
        .min()
        .unwrap_or(0)
}

/// Strip blank boundary lines from the needle, dedent both sides by their
/// own common minimum indentation, then require exact per-line equality
/// (relative indentation and internal spacing are preserved — this stage is
/// stricter than `line_trimmed` per line, but tolerates a needle that was
/// copied with stray blank lines around the block).
fn indentation_flexible_matches(
    content: &str,
    lines: &[LineSpan],
    find: &str,
) -> Vec<Range<usize>> {
    let (all_find_lines, trailing_newline) = split_find(find);
    // Trim blank boundary lines from the needle.
    let mut start_idx = 0usize;
    let mut end_idx = all_find_lines.len();
    while start_idx < end_idx && all_find_lines[start_idx].trim().is_empty() {
        start_idx += 1;
    }
    let mut trimmed_back = 0usize;
    while end_idx > start_idx && all_find_lines[end_idx - 1].trim().is_empty() {
        end_idx -= 1;
        trimmed_back += 1;
    }
    let find_lines = &all_find_lines[start_idx..end_idx];
    if find_lines.is_empty() {
        return Vec::new();
    }
    let find_indent = common_indent(find_lines.iter().copied());

    let n = find_lines.len();
    let mut out = Vec::new();
    if lines.len() < n {
        return out;
    }
    for i in 0..=(lines.len() - n) {
        let window_indent = common_indent((0..n).map(|j| lines[i + j].text(content)));
        let all_equal = find_lines.iter().enumerate().all(|(j, fl)| {
            let wl = lines[i + j].text(content);
            if fl.trim().is_empty() && wl.trim().is_empty() {
                return true; // blank lines are equal regardless of residue
            }
            strip_indent(wl, window_indent) == strip_indent(fl, find_indent)
        });
        if all_equal {
            // Only consume the file's trailing newline when the needle's
            // boundary was NOT trimmed away — otherwise the span no longer
            // corresponds to the needle's final newline.
            let consume = trailing_newline && trimmed_back == 0;
            out.push(window_range(content, lines, i, n, consume));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Stage 5: escape_normalized
// ---------------------------------------------------------------------------

/// Unescape literal `\n`, `\t`, `\r`, `\\`, `\"`, `\'` sequences. Unknown
/// escapes are kept verbatim. Also used by `edit_file` to interpret the
/// call's `new_string` (and the guard's needle) consistently when the
/// `escape_normalized` stage produced the match (#1771 review).
pub(crate) fn unescape_find(find: &str) -> String {
    let mut out = String::with_capacity(find.len());
    let mut chars = find.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some('\'') => out.push('\''),
            Some('"') => out.push('"'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn escape_normalized_matches(content: &str, find: &str) -> Vec<Range<usize>> {
    let unescaped = unescape_find(find);
    if unescaped == find {
        // Nothing was unescaped — identical to the exact stage, skip.
        return Vec::new();
    }
    exact_matches(content, &unescaped)
}

// ---------------------------------------------------------------------------
// Stage 6: block_anchor
// ---------------------------------------------------------------------------

/// Classic two-row Levenshtein edit distance (chars).
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

/// Normalized similarity in `[0, 1]` (1.0 = identical).
fn similarity(a: &str, b: &str) -> f64 {
    let max_len = a.chars().count().max(b.chars().count());
    if max_len == 0 {
        return 1.0;
    }
    1.0 - levenshtein(a, b) as f64 / max_len as f64
}

/// Average pairwise similarity of two middle-line sequences, normalized by
/// the LONGER of the two so unpaired lines on either side drag the score
/// down instead of being ignored: every line the needle described but the
/// span lacks — and every span line the needle never described — counts as
/// a 0-similarity entry (#1771 review).
fn block_similarity(middle_find: &[&str], middle_content: &[&str]) -> f64 {
    let total = middle_find.len().max(middle_content.len());
    if total == 0 {
        return 1.0;
    }
    let paired = middle_find.len().min(middle_content.len());
    (0..paired)
        .map(|k| similarity(middle_find[k], middle_content[k]))
        .sum::<f64>()
        / total as f64
}

/// Anchor on the needle's first and last trimmed lines. For each opening
/// anchor, EVERY closing anchor within reach is considered — a nested block
/// may close with a line trimmed-equal to the last anchor well before the
/// real block end, and pairing only the nearest one would splice over a
/// truncated span (#1771 review). Candidate spans are scored with
/// [`block_similarity`], so span middle lines the needle never described
/// count against the score; the best-scoring candidate per opening anchor
/// that clears the threshold wins. Spans whose middles outnumber the
/// needle's by more than `1/threshold` are pruned outright: even perfect
/// pairs cannot lift them over the bar, which also bounds the scan.
fn block_anchor_matches(content: &str, lines: &[LineSpan], find: &str) -> Vec<Range<usize>> {
    let (find_lines, trailing_newline) = split_find(find);
    if find_lines.len() < 3 {
        return Vec::new();
    }
    let first_anchor = find_lines[0].trim();
    let last_anchor = find_lines[find_lines.len() - 1].trim();
    if first_anchor.is_empty() || last_anchor.is_empty() {
        return Vec::new();
    }
    let middle_find: Vec<&str> = find_lines[1..find_lines.len() - 1]
        .iter()
        .map(|l| l.trim())
        .collect();
    // Score is capped by paired/max = middle_find/middle_content once the
    // span's middles outnumber the needle's — beyond this count it can
    // never clear the threshold.
    let max_middle = (middle_find.len() as f64 / BLOCK_ANCHOR_SIMILARITY_THRESHOLD) as usize;

    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.text(content).trim() != first_anchor {
            continue;
        }
        // Best-scoring viable closing anchor for this opening anchor; on a
        // score tie the nearest one wins (strictly-greater comparison).
        let mut best: Option<(f64, usize)> = None;
        for j in i + 1..lines.len() {
            if j - i - 1 > max_middle {
                break;
            }
            if lines[j].text(content).trim() != last_anchor {
                continue;
            }
            let middle_content: Vec<&str> =
                (i + 1..j).map(|k| lines[k].text(content).trim()).collect();
            let score = block_similarity(&middle_find, &middle_content);
            if score >= BLOCK_ANCHOR_SIMILARITY_THRESHOLD && best.is_none_or(|(s, _)| score > s) {
                best = Some((score, j));
            }
        }
        if let Some((_, j)) = best {
            let n = j - i + 1;
            out.push(window_range(content, lines, i, n, trailing_newline));
        }
    }
    out
}
