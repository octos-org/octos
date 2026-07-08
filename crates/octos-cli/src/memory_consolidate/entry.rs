//! MEMORY.md entry model: parse, render, stable ids, stamps, INIT migration.
//!
//! An entry is one block of consecutive non-blank lines. The last line of a
//! block ends with a stable id token `^m` + 6 chars of `[a-z2-7]` (base32),
//! e.g. `... (updated: 2026-07-08) ^m4k2abq`. Files where no block carries an
//! id are *legacy* and go through INIT (ids assigned, `(updated: unknown,
//! imported: <today>)` stamped). Mixed files are rejected — fail closed.

use std::collections::HashSet;

use chrono::NaiveDate;
use eyre::{Result, bail};
use sha2::{Digest, Sha256};

/// Alphabet for entry id chars (RFC 4648 base32, lowercase).
const ID_ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
/// Number of random chars after the `^m` prefix.
const ID_CHARS: usize = 6;

/// One MEMORY.md entry: a block of non-blank lines whose last line ends with
/// the entry's id token. `text` is the full block (id token included), lines
/// joined with `\n`, no trailing newline — this exact string is what gets
/// hashed, archived, and byte-identically restored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Stable id including the `^m` prefix, e.g. `^m4k2abq`.
    pub id: String,
    /// Full block text including the trailing id token.
    pub text: String,
}

/// The `(updated: …)` freshness stamp of an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdatedStamp {
    /// A real date — eligible for age-based auto-archive.
    Real(NaiveDate),
    /// `(updated: unknown, …)` from INIT — sorts oldest, NEVER auto-archived.
    Unknown,
}

/// Result of parsing MEMORY.md.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedMemory {
    /// Every block carries an id (empty file parses as zero entries).
    Entries(Vec<Entry>),
    /// No block carries an id — INIT required. Blocks in file order.
    Legacy(Vec<String>),
}

/// True when `s` is a well-formed entry id token (`^m` + 6 of `[a-z2-7]`).
pub fn is_id_token(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("^m") else {
        return false;
    };
    rest.len() == ID_CHARS && rest.bytes().all(|b| ID_ALPHABET.contains(&b))
}

/// Extract the trailing id token of a block, if the last whitespace-separated
/// token of the last line is a well-formed id.
fn trailing_id(block: &str) -> Option<&str> {
    let last_line = block.lines().next_back()?;
    let token = last_line.split_whitespace().next_back()?;
    is_id_token(token).then_some(token)
}

/// Split file content into blocks of consecutive non-blank lines.
/// Lines containing only whitespace act as separators.
fn split_blocks(content: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            if !current.is_empty() {
                blocks.push(current.join("\n"));
                current = Vec::new();
            }
        } else {
            current.push(line);
        }
    }
    if !current.is_empty() {
        blocks.push(current.join("\n"));
    }
    blocks
}

/// Parse MEMORY.md content.
///
/// - All blocks carry ids → [`ParsedMemory::Entries`]; duplicate ids reject
///   the whole file.
/// - No block carries an id → [`ParsedMemory::Legacy`] (INIT needed).
/// - A mix of id-bearing and id-less blocks is rejected (fail closed).
pub fn parse_memory_md(content: &str) -> Result<ParsedMemory> {
    let blocks = split_blocks(content);
    if blocks.is_empty() {
        return Ok(ParsedMemory::Entries(Vec::new()));
    }

    let with_ids = blocks.iter().filter(|b| trailing_id(b).is_some()).count();
    if with_ids == 0 {
        return Ok(ParsedMemory::Legacy(blocks));
    }
    if with_ids != blocks.len() {
        bail!(
            "MEMORY.md is in a mixed state: {} of {} blocks carry ^m ids — refusing to guess",
            with_ids,
            blocks.len()
        );
    }

    let mut seen: HashSet<String> = HashSet::new();
    let mut entries = Vec::with_capacity(blocks.len());
    for block in blocks {
        let id = trailing_id(&block).expect("counted above").to_string();
        if !seen.insert(id.clone()) {
            bail!("MEMORY.md contains duplicate entry id {id} — refusing to parse");
        }
        entries.push(Entry { id, text: block });
    }
    Ok(ParsedMemory::Entries(entries))
}

/// Render entries back to file content: blocks in order, one blank line
/// between blocks, trailing newline. Empty input renders as an empty string.
pub fn render_memory_md(entries: &[Entry]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let mut out = entries
        .iter()
        .map(|e| e.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    out.push('\n');
    out
}

/// Generate a fresh id not present in `taken` (collision → regenerate).
pub fn generate_id(taken: &HashSet<String>) -> String {
    loop {
        let bytes = uuid::Uuid::new_v4();
        let id = id_from_bytes(&bytes.as_bytes()[..ID_CHARS]);
        if !taken.contains(&id) {
            return id;
        }
    }
}

/// Map raw bytes onto the id alphabet. Split out for testability.
fn id_from_bytes(bytes: &[u8]) -> String {
    let mut id = String::with_capacity(2 + ID_CHARS);
    id.push_str("^m");
    for b in bytes.iter().take(ID_CHARS) {
        id.push(ID_ALPHABET[(*b as usize) % ID_ALPHABET.len()] as char);
    }
    id
}

/// INIT migration: assign a fresh id to every legacy block and stamp each
/// `(updated: unknown, imported: <today>)`. 0-loss by construction — block
/// text is only appended to, never rewritten. New ids are added to `taken`.
pub fn init_entries(
    blocks: &[String],
    today: NaiveDate,
    taken: &mut HashSet<String>,
) -> Vec<Entry> {
    blocks
        .iter()
        .map(|block| {
            let id = generate_id(taken);
            taken.insert(id.clone());
            let text = format!(
                "{block} (updated: unknown, imported: {}) {id}",
                today.format("%Y-%m-%d")
            );
            Entry { id, text }
        })
        .collect()
}

impl Entry {
    /// SHA-256 hex digest of the full entry text (hash-bound candidates,
    /// byte-identical restore verification).
    pub fn content_hash(&self) -> String {
        sha256_hex(&self.text)
    }

    /// Parse the entry's `(updated: …)` stamp. Uses the LAST occurrence in
    /// the block (stamps live at the end by convention). Returns `None` when
    /// the entry carries no stamp at all.
    pub fn updated_stamp(&self) -> Option<UpdatedStamp> {
        let mut result = None;
        let mut rest = self.text.as_str();
        while let Some(pos) = rest.find("(updated:") {
            let after = rest[pos + "(updated:".len()..].trim_start();
            if after.starts_with("unknown") {
                result = Some(UpdatedStamp::Unknown);
            } else if let Some(date) = after
                .get(..10)
                .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
            {
                result = Some(UpdatedStamp::Real(date));
            }
            rest = &rest[pos + "(updated:".len()..];
        }
        result
    }

    /// Whitespace-folded lines of the entry (for LINE-EXACT scrub matching).
    /// Empty folds are dropped.
    pub fn folded_lines(&self) -> Vec<String> {
        self.text
            .lines()
            .map(fold_whitespace)
            .filter(|l| !l.is_empty())
            .collect()
    }
}

/// Collapse runs of whitespace to single spaces and trim.
pub fn fold_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// SHA-256 hex digest of a string.
pub fn sha256_hex(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// CJK-aware token estimate: ASCII chars count 1/4 token, every non-ASCII
/// char counts a full token. Local implementation on purpose — the engine
/// must not depend on provider tokenizers.
pub fn estimate_tokens(s: &str) -> usize {
    let (ascii, non_ascii) = s.chars().fold((0usize, 0usize), |(a, n), c| {
        if c.is_ascii() { (a + 1, n) } else { (a, n + 1) }
    });
    ascii.div_ceil(4) + non_ascii
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_parse_entries_when_all_blocks_have_ids() {
        let content = "User prefers dark mode. (updated: 2026-07-01) ^mabc234\n\n\
                       Works at Acme Corp.\nSecond line here. (updated: 2026-06-01) ^mdef567\n";
        let parsed = parse_memory_md(content).unwrap();
        let ParsedMemory::Entries(entries) = parsed else {
            panic!("expected Entries");
        };
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "^mabc234");
        assert_eq!(entries[1].id, "^mdef567");
        assert!(entries[1].text.contains("Second line here."));
    }

    #[test]
    fn should_error_when_duplicate_ids() {
        let content = "A. ^mabc234\n\nB. ^mabc234\n";
        let err = parse_memory_md(content).unwrap_err();
        assert!(err.to_string().contains("duplicate"), "got: {err}");
    }

    #[test]
    fn should_detect_legacy_when_no_ids() {
        let content = "Old fact one.\n\nOld fact two\nwith two lines.\n";
        let parsed = parse_memory_md(content).unwrap();
        let ParsedMemory::Legacy(blocks) = parsed else {
            panic!("expected Legacy");
        };
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0], "Old fact one.");
    }

    #[test]
    fn should_error_when_mixed_id_and_legacy_blocks() {
        let content = "Has id. ^mabc234\n\nNo id here.\n";
        let err = parse_memory_md(content).unwrap_err();
        assert!(err.to_string().contains("mixed"), "got: {err}");
    }

    #[test]
    fn should_parse_empty_file_as_zero_entries() {
        assert_eq!(
            parse_memory_md("").unwrap(),
            ParsedMemory::Entries(Vec::new())
        );
        assert_eq!(
            parse_memory_md("  \n\n \n").unwrap(),
            ParsedMemory::Entries(Vec::new())
        );
    }

    #[test]
    fn should_roundtrip_when_rendering_parsed_entries() {
        let content = "First entry. ^mabc234\n\nSecond entry\nspanning lines. ^mdef567\n";
        let ParsedMemory::Entries(entries) = parse_memory_md(content).unwrap() else {
            panic!("expected Entries");
        };
        assert_eq!(render_memory_md(&entries), content);
    }

    #[test]
    fn should_reject_id_token_when_invalid_chars() {
        assert!(is_id_token("^mabc234"));
        assert!(is_id_token("^m4k2abq"));
        // 0, 1, 8, 9 are not in the base32 alphabet.
        assert!(!is_id_token("^mabc123"));
        assert!(!is_id_token("^mabc890"));
        assert!(!is_id_token("^mabcde")); // too short
        assert!(!is_id_token("^mabcdefg")); // too long
        assert!(!is_id_token("mabc234")); // missing ^
        assert!(!is_id_token("^mABC234")); // uppercase
    }

    #[test]
    fn should_generate_valid_unique_id_when_asked() {
        let taken = HashSet::new();
        let id = generate_id(&taken);
        assert!(is_id_token(&id), "generated invalid id: {id}");
        // Deterministic mapping check.
        assert_eq!(id_from_bytes(&[0, 1, 2, 3, 4, 5]), "^mabcdef");
    }

    #[test]
    fn should_assign_ids_and_unknown_stamps_when_init() {
        let blocks = vec!["Old fact.".to_string(), "Another\nmultiline.".to_string()];
        let today = NaiveDate::from_ymd_opt(2026, 7, 7).unwrap();
        let mut taken = HashSet::new();
        let entries = init_entries(&blocks, today, &mut taken);
        assert_eq!(entries.len(), 2);
        for (entry, block) in entries.iter().zip(&blocks) {
            assert!(entry.text.starts_with(block.as_str()), "content preserved");
            assert!(
                entry
                    .text
                    .contains("(updated: unknown, imported: 2026-07-07)")
            );
            assert!(entry.text.ends_with(&entry.id));
            assert_eq!(entry.updated_stamp(), Some(UpdatedStamp::Unknown));
        }
        assert_eq!(taken.len(), 2);
        // Re-parsing the render must produce the same entries (ids stick).
        let rendered = render_memory_md(&entries);
        let ParsedMemory::Entries(reparsed) = parse_memory_md(&rendered).unwrap() else {
            panic!("expected Entries");
        };
        assert_eq!(reparsed, entries);
    }

    #[test]
    fn should_parse_real_date_stamp_when_present() {
        let entry = Entry {
            id: "^mabc234".into(),
            text: "Fact. (updated: 2026-06-15) ^mabc234".into(),
        };
        assert_eq!(
            entry.updated_stamp(),
            Some(UpdatedStamp::Real(
                NaiveDate::from_ymd_opt(2026, 6, 15).unwrap()
            ))
        );

        let none = Entry {
            id: "^mabc234".into(),
            text: "No stamp here. ^mabc234".into(),
        };
        assert_eq!(none.updated_stamp(), None);
    }

    #[test]
    fn should_use_last_stamp_when_multiple_present() {
        let entry = Entry {
            id: "^mabc234".into(),
            text: "Quoted (updated: 2020-01-01) inside. (updated: 2026-06-15) ^mabc234".into(),
        };
        assert_eq!(
            entry.updated_stamp(),
            Some(UpdatedStamp::Real(
                NaiveDate::from_ymd_opt(2026, 6, 15).unwrap()
            ))
        );
    }

    #[test]
    fn should_estimate_tokens_cjk_aware() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1); // 4 ascii = 1
        assert_eq!(estimate_tokens("abcde"), 2); // ceil(5/4)
        assert_eq!(estimate_tokens("中文"), 2); // 1 per CJK char
        assert_eq!(estimate_tokens("ab中文"), 3); // ceil(2/4)=1 + 2
    }

    #[test]
    fn should_fold_whitespace_when_matching_lines() {
        assert_eq!(fold_whitespace("  a\t b   c "), "a b c");
        assert_eq!(fold_whitespace("   "), "");
    }

    #[test]
    fn should_hash_entry_text_when_asked() {
        let entry = Entry {
            id: "^mabc234".into(),
            text: "Fact. ^mabc234".into(),
        };
        assert_eq!(entry.content_hash(), sha256_hex("Fact. ^mabc234"));
        assert_eq!(entry.content_hash().len(), 64);
    }
}
