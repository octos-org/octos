//! Quote- and pipeline-aware pattern matching for
//! [`SafePolicy`](crate::policy::SafePolicy) (#1769, lite rescope).
//!
//! A small, pure-Rust shell tokenizer (deliberately NOT tree-sitter — that
//! would pull a C grammar against the pure-Rust ethos) that understands just
//! enough POSIX shell to make the accident-catching denylist both less noisy
//! and harder to launder:
//!
//! * **Quoting** — single/double-quoted literals are data, so dangerous text
//!   inside a quoted argument (`echo "don't rm -rf /"`) no longer
//!   false-positives, while quoting a dangerous argv (`rm "-rf" "/"`) no
//!   longer hides it.
//! * **Command position** — deny/ask patterns are anchored at the command
//!   word of each pipeline segment (split on `;`, `&&`, `||`, `|`, `&`, and
//!   subshell openers `$(`, `` ` ``, `(`). Wrapper commands (`xargs`, `env`,
//!   `sudo`, ...) forward their arguments into command position, and
//!   `sh -c '...'` scripts are analyzed recursively.
//! * **Conservative dynamics** — command substitution, `eval`,
//!   backslash-escapes outside quotes, and `$VAR`/`${VAR}` expansion in
//!   command position make a segment unanalyzable. Such segments fall back to
//!   the legacy whitespace-normalized substring match on their raw text, so
//!   the result is never LESS strict than the old matcher.
//!
//! Like `SafePolicy` itself this is **not a security boundary** — it exists
//! to catch obvious accidents; real isolation comes from the sandbox layer.

use crate::policy::Decision;

/// Maximum `sh -c` recursion depth before falling back to the legacy match.
const MAX_RECURSION_DEPTH: usize = 8;

/// How many tokens after a shell command are scanned for a `-c`-style flag.
const SHELL_FLAG_WINDOW: usize = 8;

/// Shell interpreters whose `-c` argument is itself a command line.
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "fish"];

/// Commands that execute their arguments: the following words are treated as
/// candidate command positions (`xargs rm -rf`, `env rm -rf /`, ...).
const WRAPPERS: &[&str] = &[
    "xargs", "env", "nohup", "exec", "setsid", "timeout", "nice", "ionice", "stdbuf", "command",
    "builtin", "sudo", "doas", "watch", "parallel", "busybox",
];

/// Shell keywords that are transparent for command position
/// (`if rm -rf /; then ...` — `rm` is the command).
const KEYWORDS: &[&str] = &[
    "if", "then", "else", "elif", "fi", "do", "done", "while", "until", "for", "in", "time", "!",
    "{", "}", "[", "[[",
];

/// Evaluate `command` against deny/ask pattern lists.
///
/// Deny beats Ask beats Allow; the worst decision across all pipeline
/// segments (and recursively analyzed `sh -c` scripts) wins.
pub(crate) fn evaluate(command: &str, deny: &[String], ask: &[String]) -> Decision {
    evaluate_at_depth(command, deny, ask, 0)
}

fn evaluate_at_depth(command: &str, deny: &[String], ask: &[String], depth: usize) -> Decision {
    if depth > MAX_RECURSION_DEPTH {
        return legacy_decision(command, deny, ask);
    }

    let mut worst = Decision::Allow;

    // Patterns containing separator/quote/expansion characters (e.g. the fork
    // bomb `:(){:|:&};:`) would be shredded by segmentation, so they keep the
    // exact legacy whole-string semantics.
    for pattern in deny.iter().filter(|p| is_structural(p)) {
        if legacy_matches(command, pattern) {
            return Decision::Deny;
        }
    }
    for pattern in ask.iter().filter(|p| is_structural(p)) {
        if legacy_matches(command, pattern) {
            worst = worse(worst, Decision::Ask);
        }
    }

    let Some(segments) = parse_segments(command) else {
        // Unterminated quoting — unanalyzable, keep the legacy behavior.
        return worse(worst, legacy_decision(command, deny, ask));
    };

    for segment in &segments {
        let decision = if segment.dynamic {
            // Unanalyzable segment: old substring behavior on the raw text,
            // quotes and all — never less strict than the legacy matcher.
            legacy_decision(&segment.raw, deny, ask)
        } else {
            analyze_clean_segment(segment, deny, ask, depth)
        };
        worst = worse(worst, decision);
        if worst == Decision::Deny {
            return Decision::Deny;
        }
    }
    worst
}

/// Worst-of ordering: Deny > Ask > Allow.
fn worse(a: Decision, b: Decision) -> Decision {
    fn rank(d: Decision) -> u8 {
        match d {
            Decision::Allow => 0,
            Decision::Ask => 1,
            Decision::Deny => 2,
        }
    }
    if rank(b) > rank(a) { b } else { a }
}

/// A pattern that contains shell structure characters cannot be matched
/// against tokenized segments; it keeps whole-string legacy matching.
fn is_structural(pattern: &str) -> bool {
    pattern.chars().any(|c| {
        matches!(
            c,
            ';' | '|'
                | '&'
                | '('
                | ')'
                | '{'
                | '}'
                | '`'
                | '$'
                | '\\'
                | '\''
                | '"'
                | '<'
                | '>'
                | '\n'
                | '\r'
        )
    })
}

// ---------------------------------------------------------------------------
// Legacy matching (the pre-#1769 SafePolicy behavior, used as fallback)
// ---------------------------------------------------------------------------

/// Collapse consecutive whitespace into single spaces and trim.
fn normalize_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Check if `pattern` appears in `haystack` at a word boundary.
///
/// A word boundary is start/end of string or a non-alphanumeric character.
/// This prevents "mkfs" from matching inside "unmkfsblah" or "sudo" inside
/// "pseudocode".
fn contains_at_word_boundary(haystack: &str, pattern: &str) -> bool {
    let pat_bytes = pattern.as_bytes();
    let hay_bytes = haystack.as_bytes();
    if pat_bytes.is_empty() || pat_bytes.len() > hay_bytes.len() {
        return false;
    }
    for i in 0..=(hay_bytes.len() - pat_bytes.len()) {
        if &hay_bytes[i..i + pat_bytes.len()] == pat_bytes {
            // Check left boundary: start of string or non-alphanumeric
            let left_ok = i == 0 || !hay_bytes[i - 1].is_ascii_alphanumeric();
            // Check right boundary: end of string or non-alphanumeric
            let right_ok = i + pat_bytes.len() == hay_bytes.len()
                || !hay_bytes[i + pat_bytes.len()].is_ascii_alphanumeric();
            if left_ok && right_ok {
                return true;
            }
        }
    }
    false
}

/// Whitespace-normalized word-boundary substring match (the old behavior).
fn legacy_matches(text: &str, pattern: &str) -> bool {
    contains_at_word_boundary(&normalize_whitespace(text), &normalize_whitespace(pattern))
}

/// The old `SafePolicy::check` over an arbitrary piece of text.
fn legacy_decision(text: &str, deny: &[String], ask: &[String]) -> Decision {
    for pattern in deny {
        if legacy_matches(text, pattern) {
            return Decision::Deny;
        }
    }
    for pattern in ask {
        if legacy_matches(text, pattern) {
            return Decision::Ask;
        }
    }
    Decision::Allow
}

// ---------------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Token {
    /// Unquoted value (quote characters removed, escapes resolved).
    value: String,
    /// Any part of the word was quoted.
    quoted: bool,
    /// The word contains an (unescaped) `$` expansion.
    has_dollar: bool,
}

#[derive(Debug, Default)]
struct Segment {
    /// Raw source text of the segment (quotes included, separators excluded).
    raw: String,
    /// Raw text with quoted spans blanked out — legacy matching over this
    /// preserves the old verdicts for unquoted text while letting quoted
    /// literals stop false-positiving.
    masked: String,
    tokens: Vec<Token>,
    /// Contains a construct we do not analyze (command substitution,
    /// backtick, backslash escape outside quotes).
    dynamic: bool,
}

#[derive(Default)]
struct Parser {
    segments: Vec<Segment>,
    seg: Segment,
    token: Option<Token>,
}

impl Parser {
    fn token_mut(&mut self) -> &mut Token {
        self.token.get_or_insert_with(Token::default)
    }

    /// Literal (unquoted) word character.
    fn lit(&mut self, c: char) {
        self.seg.raw.push(c);
        self.seg.masked.push(c);
        self.token_mut().value.push(c);
    }

    /// Character that belongs to the raw text but not to any token value
    /// (whitespace, redirection operators).
    fn raw_only(&mut self, c: char) {
        self.seg.raw.push(c);
        self.seg.masked.push(c);
    }

    /// A quote character itself: raw keeps it, masked blanks it.
    fn quote_mark(&mut self, c: char) {
        self.seg.raw.push(c);
        self.seg.masked.push(' ');
        self.token_mut().quoted = true;
    }

    /// Character inside a quoted span: raw keeps it, masked blanks it,
    /// the token value receives `value` (may differ for escapes).
    fn quoted_char(&mut self, raw: char, value: Option<char>) {
        self.seg.raw.push(raw);
        self.seg.masked.push(' ');
        if let Some(v) = value {
            self.token_mut().value.push(v);
        }
    }

    fn end_token(&mut self) {
        if let Some(token) = self.token.take()
            && (!token.value.is_empty() || token.quoted)
        {
            self.seg.tokens.push(token);
        }
    }

    fn end_segment(&mut self) {
        self.end_token();
        let seg = std::mem::take(&mut self.seg);
        if !seg.tokens.is_empty() || !seg.raw.trim().is_empty() {
            self.segments.push(seg);
        }
    }
}

/// Tokenize a command line into pipeline segments.
///
/// Returns `None` when the input is unanalyzable as a whole (unterminated
/// quote) — the caller then falls back to legacy matching on the full string.
fn parse_segments(input: &str) -> Option<Vec<Segment>> {
    let mut p = Parser::default();
    let mut it = input.chars().peekable();
    // For each open paren group: `true` if it was a `$(` command substitution,
    // `false` for a plain `(` subshell. A substitution splices its output into
    // the ENCLOSING command's words, so the continuation after its `)` must be
    // treated as dynamic; a plain subshell close leaves the continuation
    // analyzable.
    let mut paren_is_subst: Vec<bool> = Vec::new();
    let mut in_backtick = false;

    while let Some(c) = it.next() {
        match c {
            '\'' => {
                p.quote_mark('\'');
                loop {
                    match it.next() {
                        None => return None,
                        Some('\'') => {
                            p.quote_mark('\'');
                            break;
                        }
                        Some(ch) => p.quoted_char(ch, Some(ch)),
                    }
                }
            }
            '"' => {
                p.quote_mark('"');
                loop {
                    match it.next() {
                        None => return None,
                        Some('"') => {
                            p.quote_mark('"');
                            break;
                        }
                        Some('\\') => match it.peek() {
                            // In double quotes, backslash only escapes these.
                            Some(&e @ ('"' | '\\' | '$' | '`')) => {
                                it.next();
                                p.quoted_char('\\', None);
                                p.quoted_char(e, Some(e));
                            }
                            _ => p.quoted_char('\\', Some('\\')),
                        },
                        Some('$') => {
                            if it.peek() == Some(&'(') {
                                // Command substitution inside double quotes:
                                // unanalyzable segment.
                                p.seg.dynamic = true;
                            }
                            p.token_mut().has_dollar = true;
                            p.quoted_char('$', Some('$'));
                        }
                        Some('`') => {
                            p.seg.dynamic = true;
                            p.quoted_char('`', Some('`'));
                        }
                        Some(ch) => p.quoted_char(ch, Some(ch)),
                    }
                }
            }
            '\\' => {
                // Backslash escape outside quotes: conservative — the segment
                // becomes unanalyzable (falls back to legacy matching).
                p.seg.dynamic = true;
                if let Some(e) = it.next() {
                    p.seg.raw.push('\\');
                    p.seg.masked.push('\\');
                    p.lit(e);
                } else {
                    p.raw_only('\\');
                }
            }
            ';' => p.end_segment(),
            '&' => {
                if it.peek() == Some(&'&') {
                    it.next();
                }
                p.end_segment();
            }
            '|' => {
                if it.peek() == Some(&'|') {
                    it.next();
                }
                p.end_segment();
            }
            '(' => {
                paren_is_subst.push(false);
                p.end_segment();
            }
            ')' => {
                p.end_segment();
                if paren_is_subst.pop() == Some(true) {
                    // Text after `$( ... )` still belongs to the enclosing
                    // simple command, whose words depend on the substitution
                    // output — keep it on the legacy fallback path.
                    p.seg.dynamic = true;
                }
            }
            '`' => {
                // Backtick substitution: the segment being ended (the
                // enclosing prefix on open, the contents on close) is dynamic,
                // and after the CLOSING backtick the continuation of the
                // enclosing command is dynamic too — its words are spliced
                // from the substitution output.
                p.seg.dynamic = true;
                p.end_segment();
                if in_backtick {
                    p.seg.dynamic = true;
                }
                in_backtick = !in_backtick;
            }
            '$' => {
                if it.peek() == Some(&'(') {
                    it.next();
                    // `$(`: enclosing segment is dynamic; inner content is a
                    // fresh segment in command position (the matching `)`
                    // terminates it via the `)` separator arm).
                    p.seg.dynamic = true;
                    p.end_segment();
                    paren_is_subst.push(true);
                } else {
                    p.token_mut().has_dollar = true;
                    p.lit('$');
                }
            }
            '<' | '>' => {
                // Redirection operators delimit words but not segments.
                p.end_token();
                p.raw_only(c);
            }
            c if c.is_whitespace() => {
                // NOTE: newline is deliberately whitespace, not a separator —
                // the legacy matcher collapsed it to a space, and treating it
                // as a separator would be less strict on multi-line strings.
                p.end_token();
                p.raw_only(c);
            }
            _ => p.lit(c),
        }
    }

    p.end_segment();
    Some(p.segments)
}

// ---------------------------------------------------------------------------
// Segment analysis
// ---------------------------------------------------------------------------

/// `NAME=value` environment assignment prefix (skipped for command position).
fn is_assignment(token: &Token) -> bool {
    let bytes = token.value.as_bytes();
    let Some(eq) = token.value.find('=') else {
        return false;
    };
    if eq == 0 {
        return false;
    }
    (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        && bytes[1..eq]
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

fn is_keyword(token: &Token) -> bool {
    !token.quoted && KEYWORDS.contains(&token.value.as_str())
}

/// Join token values from `from` with single spaces, stopping once the
/// result is long enough to decide an anchored match for `limit` bytes.
fn bounded_join(tokens: &[Token], from: usize, limit: usize) -> String {
    let mut joined = String::new();
    for (i, token) in tokens[from..].iter().enumerate() {
        if i > 0 {
            joined.push(' ');
        }
        joined.push_str(&token.value);
        if joined.len() > limit {
            break;
        }
    }
    joined
}

/// Does `pattern` match at the start of the token sequence beginning at
/// `from`, ending on a word boundary?
fn anchored_match(tokens: &[Token], from: usize, pattern: &str) -> bool {
    let pattern = normalize_whitespace(pattern);
    if pattern.is_empty() {
        return false;
    }
    let joined = bounded_join(tokens, from, pattern.len());
    if !joined.starts_with(&pattern) {
        return false;
    }
    match joined.as_bytes().get(pattern.len()) {
        None => true,
        Some(b) => !b.is_ascii_alphanumeric(),
    }
}

/// Analyze a fully tokenized (non-dynamic) segment.
fn analyze_clean_segment(
    segment: &Segment,
    deny: &[String],
    ask: &[String],
    depth: usize,
) -> Decision {
    // 1) Legacy substring over the masked text: quoted literals are blanked
    //    out (fixing the false positives), unquoted text keeps every verdict
    //    the old matcher produced.
    let mut worst = legacy_decision(&segment.masked, deny, ask);
    if worst == Decision::Deny {
        return Decision::Deny;
    }

    let tokens = &segment.tokens;

    // 2) Anchored matching at every command position. Start after leading
    //    assignments/keywords; wrappers forward their arguments.
    let mut first = 0;
    while first < tokens.len() && (is_assignment(&tokens[first]) || is_keyword(&tokens[first])) {
        first += 1;
    }
    let mut queued = vec![false; tokens.len()];
    let mut queue = Vec::new();
    if first < tokens.len() {
        queued[first] = true;
        queue.push(first);
    }

    while let Some(pos) = queue.pop() {
        let token = &tokens[pos];

        // Expansion or eval in command position: unanalyzable — fall back to
        // the legacy behavior over the segment's raw text (never less strict).
        if token.has_dollar || token.value == "eval" {
            return worse(worst, legacy_decision(&segment.raw, deny, ask));
        }

        for pattern in deny {
            if anchored_match(tokens, pos, pattern) {
                return Decision::Deny;
            }
        }
        for pattern in ask {
            if anchored_match(tokens, pos, pattern) {
                worst = worse(worst, Decision::Ask);
            }
        }

        let name = token.value.rsplit('/').next().unwrap_or(&token.value);
        let is_shell = SHELLS.contains(&name);
        if is_shell {
            // `sh -c '<script>'`: the flag's argument is a nested command
            // line — analyze it recursively.
            let window_end = tokens.len().min(pos + 1 + SHELL_FLAG_WINDOW);
            for j in (pos + 1)..window_end {
                let v = &tokens[j].value;
                if v.starts_with('-')
                    && v.contains('c')
                    && let Some(script) = tokens.get(j + 1)
                {
                    worst = worse(
                        worst,
                        evaluate_at_depth(&script.value, deny, ask, depth + 1),
                    );
                    if worst == Decision::Deny {
                        return Decision::Deny;
                    }
                }
            }
        }
        if is_shell || WRAPPERS.contains(&name) {
            // Arguments of a wrapper are candidate command positions
            // (`xargs rm -rf`, `env FOO=1 rm -rf /`, `sudo sh -c ...`).
            for (j, slot) in queued.iter_mut().enumerate().skip(pos + 1) {
                if !*slot {
                    *slot = true;
                    queue.push(j);
                }
            }
        }
    }
    worst
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deny() -> Vec<String> {
        vec![
            "rm -rf /".into(),
            "rm -rf /*".into(),
            "dd if=".into(),
            "mkfs".into(),
            ":(){:|:&};:".into(),
            "chmod -R 777 /".into(),
        ]
    }

    fn ask() -> Vec<String> {
        vec![
            "sudo".into(),
            "rm -rf".into(),
            "git push --force".into(),
            "git reset --hard".into(),
        ]
    }

    fn check(cmd: &str) -> Decision {
        evaluate(cmd, &deny(), &ask())
    }

    #[test]
    fn should_split_segments_when_separators_present() {
        let segs = parse_segments("a b && c | d ; e & f").unwrap();
        let firsts: Vec<&str> = segs.iter().map(|s| s.tokens[0].value.as_str()).collect();
        assert_eq!(firsts, vec!["a", "c", "d", "e", "f"]);
    }

    #[test]
    fn should_keep_one_segment_when_separator_is_quoted() {
        let segs = parse_segments("echo \"a && b; c\"").unwrap();
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].tokens.len(), 2);
        assert_eq!(segs[0].tokens[1].value, "a && b; c");
        assert!(segs[0].tokens[1].quoted);
        assert!(!segs[0].dynamic);
    }

    #[test]
    fn should_unquote_values_when_tokens_are_quoted() {
        let segs = parse_segments("rm '-rf' \"/\"").unwrap();
        let values: Vec<&str> = segs[0].tokens.iter().map(|t| t.value.as_str()).collect();
        assert_eq!(values, vec!["rm", "-rf", "/"]);
        // Masked text blanks the quoted spans.
        assert_eq!(normalize_whitespace(&segs[0].masked), "rm");
    }

    #[test]
    fn should_mark_dynamic_when_substitution_backslash_or_backtick() {
        assert!(parse_segments("echo $(date)").unwrap()[0].dynamic);
        assert!(parse_segments("echo `date`").unwrap()[0].dynamic);
        assert!(parse_segments("echo a\\;b").unwrap()[0].dynamic);
        assert!(parse_segments("echo \"x $(date)\"").unwrap()[0].dynamic);
        // Single quotes make $( literal — not dynamic.
        assert!(!parse_segments("echo '$(date)'").unwrap()[0].dynamic);
    }

    #[test]
    fn should_return_none_when_quote_unterminated() {
        assert!(parse_segments("echo \"oops").is_none());
        assert!(parse_segments("echo 'oops").is_none());
    }

    #[test]
    fn should_flag_dollar_when_expansion_in_word() {
        let segs = parse_segments("$CMD ${X} \"$Y\"").unwrap();
        assert!(segs[0].tokens.iter().all(|t| t.has_dollar));
        // Escaped dollar inside double quotes is literal.
        let segs = parse_segments("echo \"\\$HOME\"").unwrap();
        assert!(!segs[0].tokens[1].has_dollar);
    }

    #[test]
    fn should_allow_quoted_literals_when_not_command_position() {
        assert_eq!(check("echo \"don't rm -rf /\""), Decision::Allow);
        assert_eq!(check("echo 'sudo rm -rf /'"), Decision::Allow);
    }

    #[test]
    fn should_deny_when_quotes_launder_command_position() {
        assert_eq!(check("rm \"-rf\" \"/\""), Decision::Deny);
        assert_eq!(check("\"rm\" -rf /"), Decision::Deny);
    }

    #[test]
    fn should_recurse_when_sh_dash_c_script() {
        assert_eq!(check("sh -c 'rm -rf /'"), Decision::Deny);
        assert_eq!(check("sudo bash -euxc 'rm -rf /'"), Decision::Deny);
        assert_eq!(check("sh -c 'ls -la'"), Decision::Allow);
        // Nested one level.
        assert_eq!(check("sh -c 'sh -c \"rm -rf /\"'"), Decision::Deny);
    }

    #[test]
    fn should_treat_wrapper_args_as_command_position() {
        assert_eq!(check("xargs rm -rf /"), Decision::Deny);
        assert_eq!(check("find . | xargs \"rm\" \"-rf\" \"/\""), Decision::Deny);
        assert_eq!(check("env PATH=/bin rm -rf /"), Decision::Deny);
        assert_eq!(check("timeout 5 rm -rf /"), Decision::Deny);
    }

    #[test]
    fn should_skip_assignments_and_keywords_when_finding_command() {
        assert_eq!(check("FOO=bar rm -rf /"), Decision::Deny);
        assert_eq!(check("if rm -rf /"), Decision::Deny);
    }

    #[test]
    fn should_fall_back_to_legacy_when_dynamic() {
        // Substitution in segment: quoted text is still scanned (old behavior).
        assert_eq!(check("echo \"rm -rf /\" $(date)"), Decision::Deny);
        // eval in command position: same.
        assert_eq!(check("eval 'rm -rf /'"), Decision::Deny);
        // Dollar in command position: legacy verdict for the raw text.
        assert_eq!(check("$rm -rf /"), Decision::Deny);
        // Legacy fallback still allows harmless dynamics.
        assert_eq!(check("echo \"hello $(date)\""), Decision::Allow);
    }

    #[test]
    fn should_match_structural_patterns_when_anywhere_in_string() {
        assert_eq!(check(":(){:|:&};:"), Decision::Deny);
    }

    #[test]
    fn should_stay_strict_when_text_follows_substitution_close() {
        // The enclosing simple command is unanalyzable once a command
        // substitution appears in it; the words AFTER the closing paren /
        // backtick belong to that same command and must keep the legacy
        // verdicts too (old matcher denied these), not get the quoted-literal
        // false-positive fix.
        assert_eq!(check("echo $(date) safe \"rm -rf /\""), Decision::Deny);
        assert_eq!(check("echo `date` safe \"rm -rf /\""), Decision::Deny);
        assert_eq!(check("echo foo$(date) \"rm -rf /\""), Decision::Deny);
        // A real separator after the close starts a NEW command: the fresh
        // command has no dynamics, so the quoted-literal fix applies again.
        assert_eq!(check("echo $(date); echo \"rm -rf /\""), Decision::Allow);
        // A plain subshell close is not a substitution: no output splices
        // into the enclosing command, so the continuation stays analyzable.
        assert_eq!(check("(date) echo \"rm -rf /\""), Decision::Allow);
        // Harmless dynamics still allowed by the legacy fallback.
        assert_eq!(check("echo $(date) safe"), Decision::Allow);
    }

    #[test]
    fn should_anchor_patterns_when_command_position_only() {
        // Argument position (unquoted) still caught via masked legacy parity.
        assert_eq!(check("echo rm -rf /"), Decision::Deny);
        // But anchored matching alone respects boundaries.
        assert_eq!(check("format-disk --help"), Decision::Allow);
        assert_eq!(check("mkfs.ext4 /dev/sda"), Decision::Deny);
        assert_eq!(check("unmkfs thing"), Decision::Allow);
    }

    #[test]
    fn should_keep_word_boundary_semantics_when_legacy_matching() {
        assert!(legacy_matches("run mkfs now", "mkfs"));
        assert!(!legacy_matches("unmkfsblah", "mkfs"));
        assert!(!legacy_matches("pseudocode", "sudo"));
        assert!(legacy_matches("rm\t-rf\n/", "rm -rf /"));
    }

    #[test]
    fn should_stop_recursion_when_depth_exceeded() {
        // Deeply nested sh -c falls back to legacy matching (which still
        // catches the plain-text pattern) instead of recursing forever.
        let mut cmd = "rm -rf /".to_string();
        for _ in 0..12 {
            cmd = format!("sh -c '{}'", cmd.replace('\'', ""));
        }
        assert_eq!(check(&cmd), Decision::Deny);
    }
}
