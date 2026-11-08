//! Text helpers for source excerpts and model-written reports.
//!
//! * [`truncate_chars`] trims by Unicode scalar values, not bytes. A byte cap
//!   silently gives CJK text a third of the room English gets (one Han
//!   character is three UTF-8 bytes), which is how a "1500 char" excerpt used
//!   to shrink to about 500 Chinese characters.
//! * [`looks_cut_off`] spots a model reply that stopped mid-sentence or
//!   mid-structure, for providers that don't say `finish_reason: "length"`.
//! * [`flag_uncited_sentences`] marks factual sentences that carry no `[N]`
//!   citation, so a reader (and a grounding check) can see them.

/// Marker appended to a sentence that states something without a `[N]`
/// citation.
pub const CITATION_NEEDED: &str = "[citation needed]";

/// Prefix of the one closing line a report may use to say what the sources
/// don't cover. It is exempt from the citation rule. Kept in English in every
/// output language so it can be recognised.
pub const GAPS_PREFIX: &str = "Gaps:";

/// Keep at most `max_chars` characters of `s`; append `suffix` when anything
/// was cut. Never splits a character.
pub fn truncate_chars(s: &str, max_chars: usize, suffix: &str) -> String {
    match s.char_indices().nth(max_chars) {
        None => s.to_string(),
        Some((cut, _)) => {
            let mut out = String::with_capacity(cut + suffix.len());
            out.push_str(&s[..cut]);
            out.push_str(suffix);
            out
        }
    }
}

/// Why `text` looks like it stopped before the writer finished, or `None`
/// when it ends cleanly.
///
/// Checks: empty text, an unclosed code fence, ending on a heading, and a
/// last prose line whose final character can't end a sentence (a letter,
/// digit, comma, colon or opening bracket). List items and table rows are
/// only flagged when they end on a joining character, since many writers
/// leave them unpunctuated. A closing `Gaps:` line is accepted as is.
pub fn looks_cut_off(text: &str) -> Option<&'static str> {
    let text = text.trim_end();
    if text.trim().is_empty() {
        return Some("empty reply");
    }
    if text.matches("```").count() % 2 == 1 {
        return Some("unclosed code block");
    }
    let last_line = text.lines().last().unwrap_or("").trim();
    if last_line.starts_with('#') {
        return Some("ends on a heading");
    }
    let content = strip_emphasis(last_line);
    if content.starts_with(GAPS_PREFIX) {
        return None;
    }
    let Some(last) = content.chars().last() else {
        return Some("ends on an empty emphasis marker");
    };
    let is_list_or_table = is_list_item(content) || content.starts_with('|');
    if is_joining(last) {
        return Some("ends mid-sentence");
    }
    if is_list_or_table || ends_sentence(last) {
        None
    } else {
        Some("ends mid-sentence")
    }
}

/// Mark every factual sentence that has no `[N]` citation with
/// [`CITATION_NEEDED`]. Returns the marked text and how many sentences were
/// flagged.
///
/// Exempt: headings, table rows, code blocks, the closing `Gaps:` line, and
/// short fragments (under four words, or under eight characters of CJK
/// text), which are usually headings in disguise or abbreviations the
/// sentence splitter cut off. A citation after the full stop (`… rose. [2]`)
/// counts for the sentence before it.
pub fn flag_uncited_sentences(text: &str) -> (String, usize) {
    let mut out = String::with_capacity(text.len() + 64);
    let mut flagged = 0;
    let mut in_code = false;
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_code = !in_code;
            out.push_str(line);
            continue;
        }
        if in_code
            || trimmed.is_empty()
            || trimmed.starts_with('#')
            || trimmed.starts_with('|')
            || strip_emphasis(trimmed).starts_with(GAPS_PREFIX)
        {
            out.push_str(line);
            continue;
        }
        for (start, end) in sentence_spans(line) {
            let sentence = &line[start..end];
            let content_end = start + sentence.trim_end().len();
            if !has_citation(sentence) && is_substantial(sentence) {
                out.push_str(&line[start..content_end]);
                out.push(' ');
                out.push_str(CITATION_NEEDED);
                out.push_str(&line[content_end..end]);
                flagged += 1;
            } else {
                out.push_str(sentence);
            }
        }
    }
    (out, flagged)
}

/// Byte spans covering `line` exactly, one per sentence. Trailing citations,
/// closing quotes/brackets and whitespace stay with the sentence they follow.
/// Fragments too short to be a sentence are merged into the next one.
fn sentence_spans(line: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let bytes_len = line.len();
    let mut start = 0;
    let mut chars = line.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let cjk_stop = matches!(c, '。' | '！' | '？');
        let latin_stop = matches!(c, '.' | '!' | '?')
            && chars
                .peek()
                .is_none_or(|&(_, n)| n.is_whitespace() || is_closer(n))
            && !(c == '.' && is_abbreviation(&line[..i]))
            && !next_word_is_lowercase(&line[i + c.len_utf8()..]);
        if !(cjk_stop || latin_stop) {
            continue;
        }
        let mut end = i + c.len_utf8();
        // Absorb closers, whitespace and `[N]` groups after the stop.
        loop {
            let rest = &line[end..];
            if let Some(n) = rest.chars().next().filter(|&n| is_closer(n) || n == ' ') {
                end += n.len_utf8();
                continue;
            }
            if let Some(len) = citation_len(rest) {
                end += len;
                continue;
            }
            break;
        }
        while chars.peek().is_some_and(|&(j, _)| j < end) {
            chars.next();
        }
        let piece = &line[start..end];
        if is_substantial(piece) || has_citation(piece) || end >= bytes_len {
            spans.push((start, end));
            start = end;
        }
    }
    if start < bytes_len {
        spans.push((start, bytes_len));
    }
    spans
}

/// Whether the word just before a full stop is an abbreviation rather than
/// a sentence end: initials such as `U.S` / `U.N` / `J`, or a common short
/// form (`Mr`, `Inc`, `e.g`, `vs`, …).
fn is_abbreviation(before: &str) -> bool {
    let word = before
        .rsplit(|c: char| c.is_whitespace() || c == '(' || c == '"' || c == '“')
        .next()
        .unwrap_or("");
    if word.is_empty() {
        return false;
    }
    let initials = word
        .split('.')
        .all(|p| p.chars().count() == 1 && p.chars().all(char::is_alphabetic));
    if initials {
        return true;
    }
    matches!(
        word.to_ascii_lowercase().as_str(),
        "mr" | "mrs"
            | "ms"
            | "dr"
            | "prof"
            | "sr"
            | "jr"
            | "st"
            | "inc"
            | "corp"
            | "co"
            | "ltd"
            | "no"
            | "vs"
            | "etc"
            | "e.g"
            | "i.e"
            | "approx"
            | "gov"
            | "gen"
            | "sen"
            | "rep"
            | "jan"
            | "feb"
            | "mar"
            | "apr"
            | "jun"
            | "jul"
            | "aug"
            | "sep"
            | "sept"
            | "oct"
            | "nov"
            | "dec"
    )
}

/// A sentence doesn't start with a lowercase letter, so a full stop before
/// one (`the U.N. website`) is not a sentence end.
fn next_word_is_lowercase(rest: &str) -> bool {
    rest.trim_start()
        .chars()
        .find(|c| !is_closer(*c))
        .is_some_and(char::is_lowercase)
}

/// Length in bytes of a leading `[12]` or `[1, 3]` group, if any.
fn citation_len(s: &str) -> Option<usize> {
    let inner = s.strip_prefix('[')?;
    let close = inner.find(']')?;
    let body = &inner[..close];
    let ok = !body.is_empty()
        && body.chars().any(|c| c.is_ascii_digit())
        && body
            .chars()
            .all(|c| c.is_ascii_digit() || c == ',' || c == ' ' || c == '-' || c == '–');
    ok.then_some(close + 2)
}

fn has_citation(s: &str) -> bool {
    s.char_indices()
        .any(|(i, c)| c == '[' && citation_len(&s[i..]).is_some())
}

fn is_substantial(s: &str) -> bool {
    let cjk = s.chars().filter(|&c| is_cjk(c)).count();
    if cjk > 0 {
        return cjk >= 8;
    }
    s.split_whitespace()
        .filter(|w| w.chars().any(char::is_alphanumeric))
        .count()
        >= 4
}

pub(crate) fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30FF   // kana
        | 0x3400..=0x4DBF // CJK ext A
        | 0x4E00..=0x9FFF // CJK unified
        | 0xAC00..=0xD7AF // Hangul
        | 0xF900..=0xFAFF)
}

fn is_closer(c: char) -> bool {
    matches!(
        c,
        ')' | ']' | '"' | '\'' | '”' | '’' | '」' | '』' | '）' | '】' | '》' | '*' | '_'
    )
}

fn ends_sentence(c: char) -> bool {
    matches!(c, '.' | '!' | '?' | '。' | '！' | '？' | '…' | '|') || is_closer(c)
}

fn is_joining(c: char) -> bool {
    matches!(
        c,
        ',' | '，' | '、' | ':' | '：' | ';' | '；' | '(' | '（' | '[' | '-' | '–' | '—' | '/'
    )
}

fn is_list_item(line: &str) -> bool {
    if line.starts_with("- ") || line.starts_with("* ") || line.starts_with("+ ") {
        return true;
    }
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && line[digits..].starts_with(". ")
}

fn strip_emphasis(line: &str) -> &str {
    line.trim_matches(|c| c == '*' || c == '_').trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flagging_preserves_structure_and_is_lossless_when_cited() {
        let text = "## Heading\n\n| a | b |\n\nOne claim here is cited [1]. Another one [2][3].\n";
        let (out, n) = flag_uncited_sentences(text);
        assert_eq!(n, 0);
        assert_eq!(out, text);
    }
}
