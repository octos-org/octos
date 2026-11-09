//! Pending-confirm candidate binding.
//!
//! Free-text host forget notes never authorize destruction directly; the
//! engine computes which entries they *might* mean and parks the note as
//! pending-confirm. Binding is Rust-side: normalized-substring overlap of at
//! least [`MIN_SUBSTRING_OVERLAP`] chars, or an exact token match of length
//! 6–11 (12+ tokens are covered by the substring rule). Model suggestions may
//! only ADD candidates that also pass this same check.

use super::entry::Entry;

/// Minimum normalized common-substring length that binds a candidate.
pub const MIN_SUBSTRING_OVERLAP: usize = 12;
/// Exact-token match bounds (inclusive).
const TOKEN_MIN: usize = 6;
const TOKEN_MAX: usize = 11;

/// Normalize for matching: lowercase, whitespace folded to single spaces.
fn normalize(s: &str) -> String {
    s.split_whitespace()
        .map(|w| w.to_lowercase())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Entry text with engine-added markers stripped (trailing id token,
/// `(updated: …)` stamps, `(unverified)`), so note content never binds to an
/// entry through bookkeeping tokens like the word "updated" or a date.
pub(super) fn strippable_entry_text(entry: &Entry) -> String {
    let mut text = entry.text.clone();
    if let Some(pos) = text.rfind(&entry.id) {
        text.replace_range(pos..pos + entry.id.len(), "");
    }
    // Remove all `(updated: …)` groups.
    while let Some(start) = text.find("(updated:") {
        let end = text[start..]
            .find(')')
            .map(|e| start + e + 1)
            .unwrap_or(text.len());
        text.replace_range(start..end, "");
    }
    text.replace("(unverified)", "")
}

/// Longest common substring length over chars (classic two-row DP).
fn lcs_substring_len(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() || b.is_empty() {
        return 0;
    }
    let (short, long) = if a.len() <= b.len() {
        (&a, &b)
    } else {
        (&b, &a)
    };
    let mut prev = vec![0usize; short.len() + 1];
    let mut cur = vec![0usize; short.len() + 1];
    let mut best = 0usize;
    for &lc in long.iter() {
        for (j, &sc) in short.iter().enumerate() {
            cur[j + 1] = if lc == sc { prev[j] + 1 } else { 0 };
            best = best.max(cur[j + 1]);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    best
}

/// Alphanumeric tokens of a normalized string.
fn tokens(s: &str) -> Vec<&str> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect()
}

/// Binding check between free-text note content and one entry. Returns the
/// binding score (chars of overlap) when the entry qualifies as a candidate.
pub fn binding_score(note_content: &str, entry: &Entry) -> Option<usize> {
    let note_norm = normalize(note_content);
    let entry_norm = normalize(&strippable_entry_text(entry));
    if note_norm.is_empty() || entry_norm.is_empty() {
        return None;
    }

    let lcs = lcs_substring_len(&note_norm, &entry_norm);
    if lcs >= MIN_SUBSTRING_OVERLAP {
        return Some(lcs);
    }

    let entry_tokens: std::collections::HashSet<&str> = tokens(&entry_norm)
        .into_iter()
        .filter(|t| (TOKEN_MIN..=TOKEN_MAX).contains(&t.chars().count()))
        .collect();
    tokens(&note_norm)
        .into_iter()
        .filter(|t| (TOKEN_MIN..=TOKEN_MAX).contains(&t.chars().count()))
        .filter(|t| entry_tokens.contains(t))
        .map(|t| t.chars().count())
        .max()
}

/// Compute candidate entries for a free-text forget note, ranked by binding
/// score (descending; ties keep entry order).
pub fn compute_candidates(note_content: &str, entries: &[Entry]) -> Vec<(String, usize)> {
    let mut scored: Vec<(String, usize)> = entries
        .iter()
        .filter_map(|e| binding_score(note_content, e).map(|score| (e.id.clone(), score)))
        .collect();
    scored.sort_by_key(|&(_, score)| std::cmp::Reverse(score));
    scored
}
