//! Centralized secret redaction for payloads that leave the process boundary
//! (UI protocol ledger, tool-call notifications, logs).
//!
//! Two complementary strategies share one rule set:
//!
//! * **Structural** ([`redact_secrets_in_value`]) walks a JSON value. Every
//!   non-null scalar under a credential-named key (`api_key`, `token`, `password`,
//!   `*_secret`, …) is replaced wholesale; every other string is run through
//!   the text rules. Strings that are themselves JSON documents are parsed and
//!   redacted structurally, so `{"body": "{\"api_key\":…}"}` is covered too.
//! * **Textual** ([`redact_secrets_in_text`]) is a single left-to-right byte
//!   scanner (no `regex` dependency) that recognises `Authorization`/`Bearer`
//!   headers, `KEY=value` / `--flag value` credential assignments, URL userinfo
//!   passwords, PEM private-key blocks and well-known API-key shapes
//!   (`sk-…`, `AKIA…`, `ghp_…`, `github_pat_…`, `xoxb-…`, `AIza…`, `glpat-…`,
//!   JWTs).
//!
//! Credential-named fields fail closed: a string under such a key is kept only
//! when it is demonstrably symbolic — an env-style reference (`$NAME`,
//! `${NAME}`, `%NAME%`), a `$(command)` that carries no secret, or a short
//! identifier-like placeholder (`<YOUR_API_KEY>`, `<your-api-key>`,
//! `{{ secrets.NAME }}`). A recognizable token inside any wrapper (`<sk-…>`,
//! `{{sk-…}}`, `${sk-…}`, quotes, brackets) is always redacted, in both paths.
//!
//! Redaction is idempotent: [`REDACTED_PLACEHOLDER`] never matches any rule,
//! so re-running the redactor over its own output is a no-op. Null is retained;
//! numbers and booleans are retained only outside credential-named fields.

use std::borrow::Cow;

use serde_json::{Map, Value};

/// Replacement inserted in place of every redacted secret.
pub const REDACTED_PLACEHOLDER: &str = "[REDACTED]";

/// Containers nested deeper than this are serialized and treated as opaque
/// text (redacted by the text rules) instead of being walked recursively.
const MAX_JSON_DEPTH: usize = 64;

/// Longest `user:password` userinfo section inspected after `://`.
const MAX_USERINFO_LEN: usize = 512;

/// Minimum length of a bare `Bearer <token>` value (outside an
/// `Authorization:` header) before it is treated as a credential.
const MIN_BARE_BEARER_LEN: usize = 8;

/// Longest `<placeholder>` / `{{ template }}` inner text still treated as a
/// symbolic reference under a credential-named key.
const MAX_PLACEHOLDER_LEN: usize = 48;

/// Longest `$(command)` text inspected for embedded secrets; longer
/// substitutions under a credential-named key are redacted outright.
const MAX_SUBSTITUTION_LEN: usize = 256;

/// Longest `<…>` / `{{…}}` / `${…}` / `$(…)` wrapper consumed as a single
/// unquoted value in text.
const MAX_WRAPPED_VALUE_LEN: usize = 256;

/// Leading words that mark `<…>` text as fill-in wording rather than a value
/// (`<your-api-key>`, `<insert token here>`, `<redacted>`).
const PLACEHOLDER_CUES: &[&str] = &[
    "your",
    "my",
    "our",
    "the",
    "a",
    "an",
    "insert",
    "enter",
    "replace",
    "paste",
    "put",
    "add",
    "set",
    "fill",
    "provide",
    "supply",
    "type",
    "use",
    "see",
    "change",
    "changeme",
    "replaceme",
    "example",
    "sample",
    "dummy",
    "placeholder",
    "redacted",
    "omitted",
    "hidden",
    "masked",
    "removed",
    "todo",
    "tbd",
    "fixme",
    "none",
    "unset",
    "missing",
    "empty",
    "null",
    "nil",
    "undefined",
    "not",
    "no",
    "optional",
    "required",
    "random",
    "generated",
    "auto",
];

/// Key names (lower-cased, separators removed) whose values are always secret.
const SECRET_KEY_NAMES: &[&str] = &[
    "apikey",
    "apikeys",
    "xapikey",
    "token",
    "accesstoken",
    "refreshtoken",
    "idtoken",
    "sessiontoken",
    "authtoken",
    "bearer",
    "authorization",
    "proxyauthorization",
    "auth",
    "secret",
    "secrets",
    "clientsecret",
    "secretkey",
    "privatekey",
    "password",
    "passwords",
    "passwd",
    "pwd",
    "passphrase",
    "passcode",
    "credential",
    "credentials",
    "awssecretaccesskey",
];

/// Trailing words that mark a compound key as secret (`db_password`,
/// `github_token`, `OPENAI_API_KEY`).
const SECRET_KEY_SUFFIXES: &[&str] = &["key", "token", "secret", "password"];

/// Authorization scheme words that are kept in place; the credential that
/// follows them is what gets redacted.
const AUTH_SCHEMES: &[&str] = &["bearer", "basic", "token", "digest", "negotiate", "oauth"];

/// Recursively redact secrets in a JSON value: credential-named keys get their
/// entire value replaced; string values anywhere have secret substrings
/// replaced in place.
///
/// Numbers, booleans and null are never modified. Strings under a
/// credential-named key are replaced by [`REDACTED_PLACEHOLDER`] unless they
/// are empty, already redacted, or a demonstrably symbolic reference such as
/// `${OPENAI_API_KEY}` / `<your-api-key>` (a leading `Bearer ` / `Basic `
/// scheme word stays visible). Strings that parse as a JSON object or
/// array are redacted structurally and re-serialized (only when something
/// actually changed, so untouched payloads stay byte-identical). Containers
/// nested deeper than 64 levels are serialized and treated as opaque text.
pub fn redact_secrets_in_value(value: &Value) -> Value {
    redact_value(value, 0, false)
}

/// Replace secret substrings in free text (command lines, JSON embedded in
/// strings, env assignments). Returns `Borrowed` when nothing changed.
pub fn redact_secrets_in_text(text: &str) -> Cow<'_, str> {
    let mut scanner = Scanner::new(text);
    let mut output: Option<String> = None;
    let mut copied_to = 0;
    let mut position = 0;
    while let Some(hit) = scanner.next_hit(position) {
        let buffer = output.get_or_insert_with(|| String::with_capacity(text.len()));
        buffer.push_str(&text[copied_to..hit.start]);
        buffer.push_str(REDACTED_PLACEHOLDER);
        copied_to = hit.end;
        position = hit.resume.max(hit.end);
    }
    match output {
        Some(mut buffer) => {
            buffer.push_str(&text[copied_to..]);
            Cow::Owned(buffer)
        }
        None => Cow::Borrowed(text),
    }
}

/// True when `text` contains anything [`redact_secrets_in_text`] would redact.
pub fn contains_secret(text: &str) -> bool {
    Scanner::new(text).next_hit(0).is_some()
}

// ---------------------------------------------------------------------------
// Structural (JSON) redaction
// ---------------------------------------------------------------------------

fn redact_value(value: &Value, depth: usize, under_secret: bool) -> Value {
    if depth >= MAX_JSON_DEPTH && (value.is_object() || value.is_array()) {
        if under_secret {
            return Value::String(REDACTED_PLACEHOLDER.to_owned());
        }
        return Value::String(redact_secrets_in_text(&value.to_string()).into_owned());
    }
    match value {
        Value::String(text) => redact_string(text, depth, under_secret),
        Value::Array(items) => redact_array(items, depth, under_secret),
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, item)| {
                    let secret = under_secret || is_secret_key_name(key);
                    (key.clone(), redact_value(item, depth + 1, secret))
                })
                .collect::<Map<String, Value>>(),
        ),
        Value::Number(_) | Value::Bool(_) if under_secret => {
            Value::String(REDACTED_PLACEHOLDER.to_owned())
        }
        other => other.clone(),
    }
}

/// Arrays get the same treatment as objects, plus argv-style handling: the
/// element right after a bare secret flag (`["--token", "abc"]`) is redacted.
fn redact_array(items: &[Value], depth: usize, under_secret: bool) -> Value {
    let mut output = Vec::with_capacity(items.len());
    let mut follows_secret_flag = false;
    for item in items {
        if follows_secret_flag {
            follows_secret_flag = false;
            if !item.is_string() {
                output.push(redact_value(item, depth + 1, true));
                continue;
            }
            if let Value::String(text) = item {
                if !text.starts_with('-') {
                    output.push(Value::String(redact_secret_string(text)));
                    continue;
                }
            }
        }
        if let Value::String(text) = item {
            follows_secret_flag = is_bare_secret_flag(text);
        }
        output.push(redact_value(item, depth + 1, under_secret));
    }
    Value::Array(output)
}

fn redact_string(text: &str, depth: usize, under_secret: bool) -> Value {
    if text.is_empty() || text == REDACTED_PLACEHOLDER {
        return Value::String(text.to_owned());
    }
    if depth + 1 < MAX_JSON_DEPTH {
        if let Some(parsed) = parse_embedded_json(text) {
            let redacted = redact_value(&parsed, depth + 1, under_secret);
            let rendered = if redacted == parsed {
                text.to_owned()
            } else {
                redacted.to_string()
            };
            return Value::String(rendered);
        }
    }
    if under_secret {
        return Value::String(redact_secret_string(text));
    }
    Value::String(redact_secrets_in_text(text).into_owned())
}

/// Parses `text` when it looks like (and is) a JSON object or array.
fn parse_embedded_json(text: &str) -> Option<Value> {
    let trimmed = text.trim_start();
    if !(trimmed.starts_with('{') || trimmed.starts_with('[')) {
        return None;
    }
    match serde_json::from_str::<Value>(text) {
        Ok(parsed) if parsed.is_object() || parsed.is_array() => Some(parsed),
        _ => None,
    }
}

/// `--token` / `--api-key` style flag with the value in the next argv slot.
fn is_bare_secret_flag(text: &str) -> bool {
    text.starts_with('-') && !text.contains('=') && is_secret_key_name(text)
}

// ---------------------------------------------------------------------------
// Key-name classification
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum KeyKind {
    Plain,
    Secret,
    /// `Cookie` / `Set-Cookie`: the value runs to end of line and may contain
    /// `;`, `,` and `=`.
    Cookie,
}

fn is_secret_key_name(name: &str) -> bool {
    classify_key(name) != KeyKind::Plain
}

/// Case-insensitive classification after splitting on `-`, `_`, ` `, `.` and
/// camelCase boundaries, so `api_key`, `X-API-Key`, `apiKey` and
/// `OPENAI_API_KEY` all normalize the same way. Bare `key`, `public_key`,
/// `key_id`, `keyword`, `tokenizer`, `token_count`, `max_tokens` and
/// `tokens_used` are deliberately *not* secret.
fn classify_key(name: &str) -> KeyKind {
    let tokens = key_name_tokens(name);
    let Some(last) = tokens.last() else {
        return KeyKind::Plain;
    };
    let joined = tokens.concat();
    if joined == "cookie" || joined == "setcookie" {
        return KeyKind::Cookie;
    }
    if SECRET_KEY_NAMES.contains(&joined.as_str()) {
        return KeyKind::Secret;
    }
    if tokens.len() >= 2 && SECRET_KEY_SUFFIXES.contains(&last.as_str()) {
        let penultimate = &tokens[tokens.len() - 2];
        if last == "key" && penultimate == "public" {
            return KeyKind::Plain;
        }
        return KeyKind::Secret;
    }
    KeyKind::Plain
}

fn key_name_tokens(name: &str) -> Vec<String> {
    let chars: Vec<char> = name.chars().collect();
    let mut tokens = Vec::new();
    let mut current = String::new();
    for (index, &ch) in chars.iter().enumerate() {
        if matches!(ch, '-' | '_' | ' ' | '.') {
            push_token(&mut tokens, &mut current);
            continue;
        }
        if ch.is_uppercase() && index > 0 {
            let prev = chars[index - 1];
            let next_is_lower = chars.get(index + 1).is_some_and(|c| c.is_lowercase());
            let boundary = prev.is_lowercase()
                || prev.is_ascii_digit()
                || (prev.is_uppercase() && next_is_lower);
            if boundary {
                push_token(&mut tokens, &mut current);
            }
        }
        current.extend(ch.to_lowercase());
    }
    push_token(&mut tokens, &mut current);
    tokens
}

fn push_token(tokens: &mut Vec<String>, current: &mut String) {
    if !current.is_empty() {
        tokens.push(std::mem::take(current));
    }
}

// ---------------------------------------------------------------------------
// Value heuristics shared by the structural and textual paths
// ---------------------------------------------------------------------------

/// Strings that carry no secret material at all: empty, already redacted, a
/// JSON/YAML type literal, or an all-asterisk mask.
fn is_trivially_non_secret(value: &str) -> bool {
    value.is_empty()
        || value == REDACTED_PLACEHOLDER
        || ["true", "false", "null", "none", "nil", "undefined"]
            .iter()
            .any(|literal| value.eq_ignore_ascii_case(literal))
        || value.bytes().all(|b| b == b'*')
}

/// Values that stay visible even under a credential-named key. Everything
/// else under such a key is redacted (fail closed).
fn is_exempt_secret_value(value: &str) -> bool {
    is_trivially_non_secret(value) || is_symbolic_reference(value)
}

/// `[A-Z][A-Z0-9_]*` — an environment-variable style name.
fn is_env_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.first().is_some_and(u8::is_ascii_uppercase)
        && bytes
            .iter()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'_')
}

/// Demonstrably symbolic references — the only wrapped forms kept under a
/// credential-named key:
///
/// * `$NAME`, `${NAME}`, `%NAME%` with an env-style `NAME`;
/// * `$(command)` whose command text carries no secret
///   ([`is_benign_substitution`]);
/// * `<…>` fill-in placeholders and `{{ … }}` / `${{ … }}` template
///   expressions with identifier-like inner text ([`is_template_placeholder`]).
///
/// A recognizable token inside any wrapper (`<sk-…>`, `{{sk-…}}`, `${sk-…}`)
/// is never symbolic.
fn is_symbolic_reference(value: &str) -> bool {
    if let Some(inner) = value
        .strip_prefix("${{")
        .and_then(|rest| rest.strip_suffix("}}"))
    {
        return is_template_placeholder(inner, true);
    }
    if let Some(inner) = value
        .strip_prefix("${")
        .and_then(|rest| rest.strip_suffix('}'))
    {
        return is_env_name(inner);
    }
    if let Some(inner) = value
        .strip_prefix("$(")
        .and_then(|rest| rest.strip_suffix(')'))
    {
        return is_benign_substitution(inner);
    }
    if let Some(inner) = value.strip_prefix('$') {
        return is_env_name(inner);
    }
    if let Some(inner) = value
        .strip_prefix('%')
        .and_then(|rest| rest.strip_suffix('%'))
    {
        return is_env_name(inner);
    }
    if let Some(inner) = value
        .strip_prefix('<')
        .and_then(|rest| rest.strip_suffix('>'))
    {
        return is_template_placeholder(inner, false);
    }
    if let Some(inner) = value
        .strip_prefix("{{")
        .and_then(|rest| rest.strip_suffix("}}"))
    {
        return is_template_placeholder(inner, true);
    }
    false
}

/// `$(command)` under a credential-named key is kept only when the command
/// text itself carries nothing the text rules would redact: no recognizable
/// token and no `KEY=value` / `key: value` credential assignment. Anything
/// else (or an over-long command) redacts the whole value.
fn is_benign_substitution(command: &str) -> bool {
    command.len() <= MAX_SUBSTITUTION_LEN && !contains_secret(command)
}

/// Identifier-like placeholder text. Inside `<…>` it must read as fill-in
/// wording: an env-style / ALL-CAPS name, a [`PLACEHOLDER_CUES`]-led phrase,
/// or an `xxxx` mask. Inside `{{ … }}` any identifier-like path counts as a
/// template expression. Never accepts recognizable tokens, over-long text, or
/// random-looking (digit-laden / mixed-case) words.
fn is_template_placeholder(inner: &str, expression: bool) -> bool {
    let inner = inner.trim_matches([' ', '\t']);
    if inner.is_empty() || inner.len() > MAX_PLACEHOLDER_LEN {
        return false;
    }
    if !inner
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b' '))
    {
        return false;
    }
    if contains_secret(inner) {
        return false;
    }
    if is_env_name(inner) {
        return true;
    }
    let words: Vec<&str> = inner
        .split([' ', '-', '_', '.'])
        .filter(|word| !word.is_empty())
        .collect();
    if words.is_empty() || !words.iter().all(|word| is_plain_word(word)) {
        return false;
    }
    if expression {
        return true;
    }
    words.iter().all(|word| {
        word.bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    }) || is_mask_word(words[0])
        || PLACEHOLDER_CUES.contains(&words[0].to_ascii_lowercase().as_str())
}

/// A single identifier word: letters with optional trailing digits, cased as
/// lowercase, UPPERCASE, Capitalized or camelCase. Rejects digit-laden or
/// randomly mixed-case words such as `dXNlcjpwYXNz`.
fn is_plain_word(word: &str) -> bool {
    let letters = word
        .trim_end_matches(|c: char| c.is_ascii_digit())
        .as_bytes();
    if letters.is_empty() || !letters.iter().all(u8::is_ascii_alphabetic) {
        return false;
    }
    if letters.iter().all(u8::is_ascii_lowercase) || letters.iter().all(u8::is_ascii_uppercase) {
        return true;
    }
    let mut inner_capitals = 0;
    for (index, &byte) in letters.iter().enumerate().skip(1) {
        if !byte.is_ascii_uppercase() {
            continue;
        }
        inner_capitals += 1;
        let opens_syllable = letters[index - 1].is_ascii_lowercase()
            && letters.get(index + 1).is_some_and(u8::is_ascii_lowercase);
        if !opens_syllable {
            return false;
        }
    }
    inner_capitals <= 3
}

/// `xxx` / `XXXX` masks.
fn is_mask_word(word: &str) -> bool {
    word.len() >= 3 && word.bytes().all(|b| b == b'x' || b == b'X')
}

/// If `text` starts with an authorization scheme word (`Bearer `, `Basic `,
/// …), returns the offset just past the scheme and the whitespace after it.
fn auth_scheme_end(text: &[u8]) -> Option<usize> {
    AUTH_SCHEMES.iter().find_map(|scheme| {
        let n = scheme.len();
        let has_scheme = text.len() > n
            && text[..n].eq_ignore_ascii_case(scheme.as_bytes())
            && matches!(text[n], b' ' | b'\t');
        has_scheme.then(|| {
            let mut end = n;
            while end < text.len() && matches!(text[end], b' ' | b'\t') {
                end += 1;
            }
            end
        })
    })
}

/// Replacement for a string that sits under a credential-named key: kept only
/// when it is trivially non-secret or a demonstrably symbolic reference. A
/// leading `Bearer ` / `Basic ` scheme word stays visible so headers remain
/// legible (`Bearer [REDACTED]`).
fn redact_secret_string(text: &str) -> String {
    if is_exempt_secret_value(text) {
        return text.to_owned();
    }
    if let Some(rest_start) = auth_scheme_end(text.as_bytes()) {
        if is_exempt_secret_value(&text[rest_start..]) {
            return text.to_owned();
        }
        return format!("{}{REDACTED_PLACEHOLDER}", &text[..rest_start]);
    }
    REDACTED_PLACEHOLDER.to_owned()
}

/// Heuristic for a bare `Bearer <token>` (no `Authorization:` context): real
/// tokens carry digits, mixed case, or are long; prose words do not.
fn looks_like_opaque_token(token: &str) -> bool {
    let bytes = token.as_bytes();
    if bytes.iter().any(u8::is_ascii_digit) {
        return true;
    }
    let mixed_case = bytes.iter().skip(1).any(u8::is_ascii_uppercase)
        && bytes.iter().any(u8::is_ascii_lowercase);
    (token.len() >= 12 && mixed_case) || token.len() >= 32
}

// ---------------------------------------------------------------------------
// Byte classes
// ---------------------------------------------------------------------------

/// Characters that make up a key name or a well-known token body.
fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn is_bearer_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'=' | b'+' | b'/' | b'~')
}

/// Ends an unquoted `key=value` / `--flag value` credential.
fn is_value_terminator(b: u8) -> bool {
    b.is_ascii_whitespace()
        || matches!(
            b,
            b'"' | b'\''
                | b'`'
                | b','
                | b';'
                | b'&'
                | b'|'
                | b'<'
                | b'>'
                | b'('
                | b')'
                | b'['
                | b']'
                | b'{'
                | b'}'
                | b'\\'
        )
}

/// Cookie headers keep `;`, `,`, `=` and spaces; they end at the line.
fn is_cookie_terminator(b: u8) -> bool {
    matches!(b, b'\n' | b'\r' | b'"' | b'\'' | b'`' | b'\\')
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

// ---------------------------------------------------------------------------
// Text scanner
// ---------------------------------------------------------------------------

/// A redaction span: `[start, end)` is replaced by the placeholder and
/// scanning continues at `resume` (`>= end`).
struct Hit {
    start: usize,
    end: usize,
    resume: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Quote {
    Double,
    Single,
    /// `\"` — a JSON string that was itself escaped into a string.
    EscapedDouble,
}

impl Quote {
    fn len(self) -> usize {
        match self {
            Quote::Double | Quote::Single => 1,
            Quote::EscapedDouble => 2,
        }
    }

    fn slot(self) -> usize {
        match self {
            Quote::Double => 0,
            Quote::Single => 1,
            Quote::EscapedDouble => 2,
        }
    }
}

/// Left-to-right scanner. Every rule is tried at each byte offset; all
/// spans it produces start and end on ASCII bytes (or at the text end), so
/// slicing the `&str` at them is always valid.
struct Scanner<'a> {
    text: &'a str,
    bytes: &'a [u8],
    /// Earliest offset from which a closing quote of each kind is known to be
    /// absent (memo that keeps unclosed-quote scans linear).
    unclosed_from: [usize; 3],
}

impl<'a> Scanner<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            bytes: text.as_bytes(),
            unclosed_from: [usize::MAX; 3],
        }
    }

    fn next_hit(&mut self, from: usize) -> Option<Hit> {
        let len = self.bytes.len();
        (from..len).find_map(|offset| self.try_at(offset))
    }

    fn try_at(&mut self, offset: usize) -> Option<Hit> {
        let dispatched = match self.bytes[offset] {
            b'-' => self.try_pem_block(offset),
            b's' | b'A' | b'g' | b'x' | b'e' => self.try_known_token(offset),
            b':' => self.try_url_userinfo(offset),
            _ => None,
        };
        if dispatched.is_some() {
            return dispatched;
        }
        if let Some(hit) = self.try_key_value(offset) {
            return Some(hit);
        }
        self.try_bare_bearer(offset)
    }

    // -- primitives ---------------------------------------------------------

    fn scan_while(&self, from: usize, pred: impl Fn(u8) -> bool) -> usize {
        let len = self.bytes.len();
        let mut offset = from.min(len);
        while offset < len && pred(self.bytes[offset]) {
            offset += 1;
        }
        offset
    }

    fn skip_hws(&self, from: usize) -> usize {
        self.scan_while(from, |b| matches!(b, b' ' | b'\t'))
    }

    fn run_at_least(&self, from: usize, min: usize, pred: impl Fn(u8) -> bool) -> Option<usize> {
        let end = self.scan_while(from, pred);
        (end.saturating_sub(from) >= min).then_some(end)
    }

    fn quote_at(&self, offset: usize) -> Option<Quote> {
        match self.bytes.get(offset)? {
            b'"' => Some(Quote::Double),
            b'\'' => Some(Quote::Single),
            b'\\' if self.bytes.get(offset + 1) == Some(&b'"') => Some(Quote::EscapedDouble),
            _ => None,
        }
    }

    fn find_closing_quote(&mut self, from: usize, quote: Quote) -> Option<usize> {
        let slot = quote.slot();
        if from >= self.unclosed_from[slot] {
            return None;
        }
        let bytes = self.bytes;
        let mut offset = from;
        let found = loop {
            if offset >= bytes.len() {
                break None;
            }
            match quote {
                Quote::Double | Quote::Single => {
                    let closing = if quote == Quote::Double { b'"' } else { b'\'' };
                    if bytes[offset] == b'\\' {
                        offset += 2;
                        continue;
                    }
                    if bytes[offset] == closing {
                        break Some(offset);
                    }
                }
                Quote::EscapedDouble => {
                    if bytes[offset] == b'\\'
                        && bytes.get(offset + 1) == Some(&b'"')
                        && (offset == 0 || bytes[offset - 1] != b'\\')
                    {
                        break Some(offset);
                    }
                }
            }
            offset += 1;
        };
        if found.is_none() {
            self.unclosed_from[slot] = self.unclosed_from[slot].min(from);
        }
        found
    }

    /// Skips a leading `Bearer ` / `Basic ` / … scheme word (kept visible)
    /// and the whitespace after it, staying inside `[from, limit)`.
    fn skip_auth_scheme(&self, from: usize, limit: usize) -> usize {
        auth_scheme_end(&self.bytes[from..limit]).map_or(from, |n| from + n)
    }

    fn scan_unquoted_value(&self, from: usize, cookie: bool) -> usize {
        if cookie {
            let mut end = self.scan_while(from, |b| !is_cookie_terminator(b));
            while end > from && matches!(self.bytes[end - 1], b' ' | b'\t') {
                end -= 1;
            }
            return end;
        }
        self.scan_wrapped_value(from)
            .unwrap_or_else(|| self.scan_while(from, |b| !is_value_terminator(b)))
    }

    /// `<…>`, `{{…}}`, `${…}`, `${{…}}` and `$(…)` are consumed as one value
    /// (same line, no quotes, bounded length) so the exemption decision sees
    /// the whole wrapper: `<sk-…>` is then redacted as a unit rather than
    /// being split at the bracket.
    fn scan_wrapped_value(&self, from: usize) -> Option<usize> {
        let rest = &self.bytes[from..];
        let (open, close): (usize, &[u8]) = if rest.starts_with(b"${{") {
            (3, b"}}")
        } else if rest.starts_with(b"${") {
            (2, b"}")
        } else if rest.starts_with(b"$(") {
            (2, b")")
        } else if rest.starts_with(b"{{") {
            (2, b"}}")
        } else if rest.starts_with(b"<") {
            (1, b">")
        } else if rest.starts_with(b"[") {
            // `token=[abc123]` / `password={abc123}` / `secret=(abc123)`:
            // the bracket used to terminate the value at width zero, so the
            // exempt-empty rule let the credential through untouched.
            (1, b"]")
        } else if rest.starts_with(b"{") {
            (1, b"}")
        } else if rest.starts_with(b"(") {
            (1, b")")
        } else {
            return None;
        };
        let limit = rest.len().min(MAX_WRAPPED_VALUE_LEN);
        let mut cursor = open;
        while cursor < limit {
            if rest[cursor..].starts_with(close) {
                return Some(from + cursor + close.len());
            }
            if matches!(rest[cursor], b'\n' | b'\r' | b'"' | b'\'' | b'`') {
                return None;
            }
            cursor += 1;
        }
        None
    }

    // -- rules --------------------------------------------------------------

    /// `key=value`, `key: value`, `"key": "value"`, `--flag value`,
    /// `Authorization: Bearer value` — for credential-named keys only.
    fn try_key_value(&mut self, offset: usize) -> Option<Hit> {
        let quote = self.quote_at(offset);
        let key_start = offset + quote.map_or(0, Quote::len);
        if quote.is_none() && offset > 0 && is_ident_byte(self.bytes[offset - 1]) {
            return None;
        }
        let key_end = self.scan_while(key_start, is_ident_byte);
        if key_end == key_start {
            return None;
        }
        let mut cursor = key_end;
        if let Some(q) = quote {
            if self.quote_at(cursor) != Some(q) {
                return None;
            }
            cursor += q.len();
        }
        let key = &self.text[key_start..key_end];
        let is_flag = key.starts_with('-');
        // `curl -u user:password` / `--user` / `--proxy-user`: the value is a
        // `user:password` pair whose password half is the credential.
        let basic_auth_flag = is_flag && matches!(key, "-u" | "--user" | "--proxy-user");
        let after_ws = self.skip_hws(cursor);
        let (value_start, space_separated) = match self.bytes.get(after_ws) {
            Some(b'=' | b':') => (self.skip_hws(after_ws + 1), false),
            Some(_) if is_flag && after_ws > cursor => (after_ws, true),
            _ => return None,
        };
        let kind = if basic_auth_flag {
            KeyKind::Secret
        } else {
            classify_key(key)
        };
        if kind == KeyKind::Plain {
            return None;
        }
        let len = self.bytes.len();
        let mut value_start = self.skip_auth_scheme(value_start, len);
        let (start, end, resume) = 'span: {
            if let Some(vq) = self.quote_at(value_start) {
                let content_start = value_start + vq.len();
                if let Some(close) = self.find_closing_quote(content_start, vq) {
                    let start = self.skip_auth_scheme(content_start, close);
                    break 'span (start, close, close + vq.len());
                }
                value_start = self.skip_auth_scheme(content_start, len);
            }
            let end = self.scan_unquoted_value(value_start, kind == KeyKind::Cookie);
            let value = &self.text[value_start..end];
            if space_separated && value.starts_with('-') {
                return None;
            }
            // `let token = tokenizer.next();` — an identifier immediately
            // followed by a call is code, not a credential value.
            if !basic_auth_flag
                && self.bytes.get(end) == Some(&b'(')
                && value.bytes().all(|b| is_ident_byte(b) || b == b'.')
            {
                return None;
            }
            (value_start, end, end)
        };
        let (start, end) = if basic_auth_flag {
            // Keep the user name visible; only the password half is secret.
            let colon = self.text[start..end].find(':')?;
            (start + colon + 1, end)
        } else {
            (start, end)
        };
        if is_exempt_secret_value(&self.text[start..end]) {
            return None;
        }
        Some(Hit { start, end, resume })
    }

    /// Standalone `Bearer <token>` without an `Authorization:` prefix.
    fn try_bare_bearer(&self, offset: usize) -> Option<Hit> {
        let bytes = self.bytes;
        let word_end = offset + 6;
        if word_end >= bytes.len() || !bytes[offset..word_end].eq_ignore_ascii_case(b"bearer") {
            return None;
        }
        if offset > 0 && is_word_byte(bytes[offset - 1]) {
            return None;
        }
        let start = self.skip_hws(word_end);
        if start == word_end {
            return None;
        }
        let mut end = self.scan_while(start, is_bearer_token_byte);
        while end > start && bytes[end - 1] == b'.' {
            end -= 1;
        }
        let token = &self.text[start..end];
        if token.len() < MIN_BARE_BEARER_LEN || !looks_like_opaque_token(token) {
            return None;
        }
        Some(Hit {
            start,
            end,
            resume: end,
        })
    }

    /// Well-known credential shapes, redacted as a whole token.
    fn try_known_token(&self, offset: usize) -> Option<Hit> {
        let bytes = self.bytes;
        if offset > 0 && is_ident_byte(bytes[offset - 1]) {
            return None;
        }
        let rest = &bytes[offset..];
        let end = if rest.starts_with(b"sk-") {
            self.run_at_least(offset + 3, 16, is_ident_byte)?
        } else if rest.starts_with(b"AKIA") {
            let body_ok = rest.len() >= 20
                && rest[4..20]
                    .iter()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit());
            if !body_ok || rest.get(20).is_some_and(u8::is_ascii_alphanumeric) {
                return None;
            }
            offset + 20
        } else if matches!(
            rest,
            [b'g', b'h', b'p' | b'o' | b'u' | b's' | b'r', b'_', ..]
        ) {
            self.run_at_least(offset + 4, 20, |b| b.is_ascii_alphanumeric())?
        } else if rest.starts_with(b"github_pat_") {
            self.run_at_least(offset + 11, 20, is_word_byte)?
        } else if matches!(
            rest,
            [b'x', b'o', b'x', b'a' | b'b' | b'p' | b'r' | b's', b'-', ..]
        ) {
            self.run_at_least(offset + 5, 10, |b| b.is_ascii_alphanumeric() || b == b'-')?
        } else if rest.starts_with(b"AIza") {
            self.run_at_least(offset + 4, 35, is_ident_byte)?
        } else if rest.starts_with(b"glpat-") {
            self.run_at_least(offset + 6, 20, is_ident_byte)?
        } else if rest.starts_with(b"eyJ") {
            self.jwt_end(offset)?
        } else {
            return None;
        };
        Some(Hit {
            start: offset,
            end,
            resume: end,
        })
    }

    /// `eyJ<header>.<payload>.<signature>` (base64url segments).
    fn jwt_end(&self, offset: usize) -> Option<usize> {
        let header_end = self.scan_while(offset, is_ident_byte);
        if header_end <= offset + 3 || self.bytes.get(header_end) != Some(&b'.') {
            return None;
        }
        let payload_end = self.scan_while(header_end + 1, is_ident_byte);
        if payload_end == header_end + 1 || self.bytes.get(payload_end) != Some(&b'.') {
            return None;
        }
        let signature_end = self.scan_while(payload_end + 1, is_ident_byte);
        (signature_end > payload_end + 1).then_some(signature_end)
    }

    /// `scheme://user:password@host` — the password part only.
    fn try_url_userinfo(&self, offset: usize) -> Option<Hit> {
        let bytes = self.bytes;
        if !bytes[offset..].starts_with(b"://") {
            return None;
        }
        let userinfo_start = offset + 3;
        let limit = bytes.len().min(userinfo_start + MAX_USERINFO_LEN);
        let mut colon = None;
        let mut cursor = userinfo_start;
        while cursor < limit {
            match bytes[cursor] {
                b'@' => break,
                b':' => {
                    if colon.is_none() {
                        colon = Some(cursor);
                    }
                }
                b'/' | b'?' | b'#' | b'"' | b'\'' | b'`' | b'<' | b'>' | b'\\' => return None,
                b if b.is_ascii_whitespace() => return None,
                _ => {}
            }
            cursor += 1;
        }
        if cursor >= limit {
            return None;
        }
        let start = colon? + 1;
        if is_exempt_secret_value(&self.text[start..cursor]) {
            return None;
        }
        Some(Hit {
            start,
            end: cursor,
            resume: cursor,
        })
    }

    /// `-----BEGIN … PRIVATE KEY-----` through the matching `-----END …-----`
    /// (or to the end of the text when the footer is missing).
    fn try_pem_block(&self, offset: usize) -> Option<Hit> {
        const BEGIN: &[u8] = b"-----BEGIN ";
        const END: &[u8] = b"-----END ";
        let header_end = self.pem_marker_end(offset, BEGIN)?;
        let mut search = header_end;
        while let Some(found) = find_subslice(&self.bytes[search..], END) {
            let footer = search + found;
            if let Some(end) = self.pem_marker_end(footer, END) {
                return Some(Hit {
                    start: offset,
                    end,
                    resume: end,
                });
            }
            search = footer + 1;
        }
        let end = self.bytes.len();
        Some(Hit {
            start: offset,
            end,
            resume: end,
        })
    }

    /// If `offset` starts `<marker><LABEL> PRIVATE KEY-----`, returns the
    /// offset just past the closing dashes.
    fn pem_marker_end(&self, offset: usize, marker: &[u8]) -> Option<usize> {
        if !self.bytes[offset..].starts_with(marker) {
            return None;
        }
        let label_start = offset + marker.len();
        let label_end = self.scan_while(label_start, |b| b.is_ascii_uppercase() || b == b' ');
        let label = &self.text[label_start..label_end];
        if !label.ends_with("PRIVATE KEY") || !self.bytes[label_end..].starts_with(b"-----") {
            return None;
        }
        Some(label_end + 5)
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use serde_json::json;

    use super::{
        REDACTED_PLACEHOLDER, contains_secret, redact_secrets_in_text, redact_secrets_in_value,
    };

    const OPENAI_KEY: &str = "sk-proj-abcdefghijklmnopqrstuvwxyz0123456789";
    const LIVE_KEY: &str = "sk-live-XXXXXXXXXXXXXXXXXXXXXXXX";

    fn text(input: &str) -> String {
        redact_secrets_in_text(input).into_owned()
    }

    // --- 1. credential field names -------------------------------------------------

    #[test]
    fn should_replace_string_values_when_key_is_credential_named() {
        let names = [
            "api_key",
            "apikey",
            "api-key",
            "x-api-key",
            "apiKey",
            "X-API-KEY",
            "token",
            "access_token",
            "refresh_token",
            "id_token",
            "session_token",
            "auth_token",
            "bearer",
            "authorization",
            "Authorization",
            "auth",
            "secret",
            "client_secret",
            "clientSecret",
            "secret_key",
            "private_key",
            "password",
            "passwd",
            "pwd",
            "credential",
            "credentials",
            "cookie",
            "set-cookie",
            "Set-Cookie",
            "aws_secret_access_key",
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "github_token",
            "webhook_secret",
            "db_password",
        ];
        for name in names {
            let input = json!({ name: "hunter2" });
            let expected = json!({ name: REDACTED_PLACEHOLDER });
            assert_eq!(redact_secrets_in_value(&input), expected, "key {name}");
        }
    }

    #[test]
    fn should_keep_values_when_key_is_not_credential_named() {
        let input = json!({
            "public_key": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGxmemVjb2Rl",
            "publicKey": "pk-value",
            "key_id": "kid-42",
            "keyword": "token",
            "tokenizer": "gpt2",
            "token_count": 12,
            "max_tokens": 4096,
            "tokens_used": 7,
            "key": "theme",
            "model": "gpt-4o",
            "path": "src/main.rs"
        });
        assert_eq!(redact_secrets_in_value(&input), input);
    }

    #[test]
    fn should_redact_non_null_scalars_when_key_is_credential_named() {
        let input = json!({
            "token": 42,
            "secret": false,
            "password": null,
            "api_key": 3.5,
            "auth": true
        });
        assert_eq!(
            redact_secrets_in_value(&input),
            json!({
                "token": REDACTED_PLACEHOLDER,
                "secret": REDACTED_PLACEHOLDER,
                "password": null,
                "api_key": REDACTED_PLACEHOLDER,
                "auth": REDACTED_PLACEHOLDER
            })
        );
    }

    #[test]
    fn should_redact_string_leaves_when_nested_under_credential_named_key() {
        let input = json!({
            "credentials": { "username": "alice", "password": "hunter2", "ttl": 30 },
            "auth": ["opaque-token-value", 1, true, null]
        });
        let expected = json!({
            "credentials": {
                "username": REDACTED_PLACEHOLDER,
                "password": REDACTED_PLACEHOLDER,
                "ttl": REDACTED_PLACEHOLDER
            },
            "auth": [REDACTED_PLACEHOLDER, REDACTED_PLACEHOLDER, REDACTED_PLACEHOLDER, null]
        });
        assert_eq!(redact_secrets_in_value(&input), expected);
    }

    #[test]
    fn should_keep_env_references_and_empty_strings_when_key_is_credential_named() {
        let input = json!({
            "api_key": "${OPENAI_API_KEY}",
            "token": "$GITHUB_TOKEN",
            "password": "",
            "secret": "<your-secret>"
        });
        assert_eq!(redact_secrets_in_value(&input), input);
    }

    #[test]
    fn should_redact_numeric_credentials_in_embedded_json_and_argv() {
        let input = json!({
            "body": "{\"passcode\":123456,\"retry_count\":2}",
            "args": ["--password", 987654, "--verbose", true, "--token", false],
            "retry_count": 3
        });
        let output = redact_secrets_in_value(&input);
        let body: serde_json::Value =
            serde_json::from_str(output["body"].as_str().unwrap()).unwrap();
        assert_eq!(
            body,
            json!({"passcode": REDACTED_PLACEHOLDER, "retry_count": 2})
        );
        assert_eq!(
            output["args"],
            json!([
                "--password",
                REDACTED_PLACEHOLDER,
                "--verbose",
                true,
                "--token",
                REDACTED_PLACEHOLDER
            ])
        );
        assert_eq!(output["retry_count"], 3);
        assert_eq!(redact_secrets_in_value(&output), output);
    }

    #[test]
    fn should_redact_value_following_secret_flag_when_argv_array_splits_flag_and_value() {
        let input = json!({
            "cmd": "tool",
            "args": ["--token", "abc123", "--verbose", "--password=hunter2", "-o", "out.txt"]
        });
        let expected = json!({
            "cmd": "tool",
            "args": [
                "--token",
                "[REDACTED]",
                "--verbose",
                "--password=[REDACTED]",
                "-o",
                "out.txt"
            ]
        });
        assert_eq!(redact_secrets_in_value(&input), expected);
    }

    // --- 2. Authorization / Bearer / env-style assignments in text ------------------

    #[test]
    fn should_redact_bearer_and_basic_credentials_when_authorization_header_in_text() {
        assert_eq!(
            text("Authorization: Bearer abc.def.ghi"),
            "Authorization: Bearer [REDACTED]"
        );
        assert_eq!(
            text("Authorization: Basic dXNlcjpwYXNz"),
            "Authorization: Basic [REDACTED]"
        );
        assert_eq!(
            text("authorization: bearer tok_12345"),
            "authorization: bearer [REDACTED]"
        );
        assert_eq!(
            text(r#"curl -H "Authorization: Bearer tok_12345" https://x.test/"#),
            r#"curl -H "Authorization: Bearer [REDACTED]" https://x.test/"#
        );
        assert_eq!(
            text(r#"{"Authorization": "Basic dXNlcjpwYXNz"}"#),
            r#"{"Authorization": "Basic [REDACTED]"}"#
        );
        assert_eq!(
            text("use Bearer abcdef123456 for auth"),
            "use Bearer [REDACTED] for auth"
        );
    }

    #[test]
    fn should_redact_header_flag_and_env_assignment_values_when_in_command_text() {
        assert_eq!(
            text("--header 'x-api-key: 0123456789abcdef'"),
            "--header 'x-api-key: [REDACTED]'"
        );
        assert_eq!(text("api_key=abc123"), "api_key=[REDACTED]");
        assert_eq!(
            text(&format!(
                "API_KEY=abc123 OPENAI_API_KEY={OPENAI_KEY} export FOO_TOKEN=bar"
            )),
            "API_KEY=[REDACTED] OPENAI_API_KEY=[REDACTED] export FOO_TOKEN=[REDACTED]"
        );
        assert_eq!(
            text("tool --api-key abc123 --token xyz789 --password hunter2 --verbose"),
            "tool --api-key [REDACTED] --token [REDACTED] --password [REDACTED] --verbose"
        );
        assert_eq!(text("password = 'hunter2'"), "password = '[REDACTED]'");
        assert_eq!(
            text("Cookie: session=abc123; theme=dark"),
            "Cookie: [REDACTED]"
        );
        assert_eq!(
            text(r#"{\"token\": \"abc123\"}"#),
            r#"{\"token\": \"[REDACTED]\"}"#
        );
        assert_eq!(
            text("git clone https://alice:hunter2@github.com/org/repo.git"),
            "git clone https://alice:[REDACTED]@github.com/org/repo.git"
        );
    }

    // --- 3. recognizable API-key shapes ----------------------------------------------

    #[test]
    fn should_redact_whole_token_when_text_contains_recognizable_api_key() {
        let cases: Vec<(String, &str)> = vec![
            (format!("key {OPENAI_KEY} end"), "key [REDACTED] end"),
            (
                "sk-ant-api03-abcdefghijklmnopqrstuvwxyz".into(),
                "[REDACTED]",
            ),
            (format!("x={LIVE_KEY}"), "x=[REDACTED]"),
            ("sk-or-v1-0123456789abcdef0123456789".into(), "[REDACTED]"),
            ("AKIAIOSFODNN7EXAMPLE".into(), "[REDACTED]"),
            (
                "ghp_abcdefghijklmnopqrstuvwxyz0123456789".into(),
                "[REDACTED]",
            ),
            (
                "gho_abcdefghijklmnopqrstuvwxyz0123456789".into(),
                "[REDACTED]",
            ),
            (
                "ghu_abcdefghijklmnopqrstuvwxyz0123456789".into(),
                "[REDACTED]",
            ),
            (
                "ghs_abcdefghijklmnopqrstuvwxyz0123456789".into(),
                "[REDACTED]",
            ),
            (
                "ghr_abcdefghijklmnopqrstuvwxyz0123456789".into(),
                "[REDACTED]",
            ),
            (
                "github_pat_11ABCDEFG0123456789abcdefghijklmnop".into(),
                "[REDACTED]",
            ),
            ("xoxb-1234567890-abcdefghij".into(), "[REDACTED]"),
            ("xoxp-1234567890-abcdefghij".into(), "[REDACTED]"),
            ("xoxa-1234567890-abcdefghij".into(), "[REDACTED]"),
            ("xoxr-1234567890-abcdefghij".into(), "[REDACTED]"),
            ("xoxs-1234567890-abcdefghij".into(), "[REDACTED]"),
            (
                "AIzaSyA1234567890abcdefghijklmnopqrstuv".into(),
                "[REDACTED]",
            ),
            ("glpat-abcdefghijklmnopqrstuvwxyz".into(), "[REDACTED]"),
            (
                "jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0In0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c".into(),
                "jwt [REDACTED]",
            ),
            (
                "cat > key.pem <<EOF\n-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBg\n-----END PRIVATE KEY-----\nEOF".into(),
                "cat > key.pem <<EOF\n[REDACTED]\nEOF",
            ),
            (
                "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXkt\n".into(),
                "[REDACTED]",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(text(&input), expected, "input {input:?}");
            assert!(contains_secret(&input), "input {input:?}");
        }
    }

    // --- 4. JSON nested inside strings -----------------------------------------------

    #[test]
    fn should_redact_structurally_when_string_value_is_embedded_json() {
        let body = format!(r#"{{"api_key":"{OPENAI_KEY}","model":"gpt-4"}}"#);
        let input = json!({ "body": body });
        let expected = json!({ "body": r#"{"api_key":"[REDACTED]","model":"gpt-4"}"# });
        assert_eq!(redact_secrets_in_value(&input), expected);

        let input = json!({ "items": r#"[{"token":"abc123"},{"count":1}]"# });
        let expected = json!({ "items": r#"[{"token":"[REDACTED]"},{"count":1}]"# });
        assert_eq!(redact_secrets_in_value(&input), expected);
    }

    #[test]
    fn should_keep_original_formatting_when_embedded_json_has_no_secrets() {
        let input = json!({ "body": "{ \"model\" : \"gpt-4\", \"max_tokens\" : 4096 }" });
        assert_eq!(redact_secrets_in_value(&input), input);
    }

    #[test]
    fn should_apply_text_rules_when_string_contains_json_fragment_but_is_not_json() {
        assert_eq!(
            text(r#"payload = {"api_key": "abc123"} and api_key=xyz"#),
            r#"payload = {"api_key": "[REDACTED]"} and api_key=[REDACTED]"#
        );
        let input = json!({ "note": "config: {\"client_secret\": \"s3cr3t\"} done" });
        let expected = json!({ "note": "config: {\"client_secret\": \"[REDACTED]\"} done" });
        assert_eq!(redact_secrets_in_value(&input), expected);
    }

    // --- required end-to-end example (NEW-2) -----------------------------------------

    #[test]
    fn should_redact_env_and_command_secrets_when_tool_arguments_carry_openai_key() {
        let input = json!({
            "env": { "OPENAI_API_KEY": OPENAI_KEY },
            "cmd": format!("curl -H 'Authorization: Bearer {LIVE_KEY}' https://api.example/v1"),
            "path": "src/main.rs",
            "max_tokens": 4096
        });
        let expected = json!({
            "env": { "OPENAI_API_KEY": "[REDACTED]" },
            "cmd": "curl -H 'Authorization: Bearer [REDACTED]' https://api.example/v1",
            "path": "src/main.rs",
            "max_tokens": 4096
        });
        let out = redact_secrets_in_value(&input);
        assert_eq!(out, expected);
        let serialized = out.to_string();
        assert!(!serialized.contains(OPENAI_KEY));
        assert!(!serialized.contains(LIVE_KEY));
        assert!(serialized.contains("https://api.example/v1"));
        assert!(!contains_secret(&serialized));
    }

    // --- 5. ordinary arguments untouched --------------------------------------------

    #[test]
    fn should_return_borrowed_identical_text_when_input_has_no_secrets() {
        let samples = [
            "the token bucket algorithm uses a keyboard and a monkey; max_tokens: 4096, token_count: 12",
            "path /home/user/.aws/credentials and ~/.ssh/id_rsa.pub",
            "https://api.example.com/v1/keys?keyword=x&page=2",
            "https://user@host.example/repo.git",
            "echo $OPENAI_API_KEY && echo ${GITHUB_TOKEN}",
            "Authorization: Bearer ${TOKEN}",
            "--token <your-token> --api-key",
            "Content-Type: application/json\nX-Request-Id: 1234",
            "desk-lamp-1234567890123456 sk-short tokenizer=gpt2 key_id=kid-42",
            "Bearer tokens are used for authentication-related flows",
            "fn read_token_count(max_tokens: usize) -> usize { max_tokens }",
            "",
            "   ",
            "[REDACTED]",
        ];
        for sample in samples {
            let out = redact_secrets_in_text(sample);
            assert!(
                matches!(out, Cow::Borrowed(_)),
                "expected untouched: {sample:?} -> {out:?}"
            );
            assert_eq!(out.as_ref(), sample);
            assert!(!contains_secret(sample), "false positive: {sample:?}");
        }
    }

    #[test]
    fn should_keep_value_byte_identical_when_arguments_are_ordinary() {
        let input = json!({
            "path": "src/main.rs",
            "content": "fn main() {\n    println!(\"hello\");\n}\n",
            "command": "cargo test -p octos-core -- --nocapture",
            "url": "https://api.example.com/v1/models",
            "max_tokens": 4096,
            "token_count": 12,
            "enabled": true,
            "nested": { "list": [1, 2, "three"], "empty": null },
            "json_text": "{ \"a\" : 1 }"
        });
        assert_eq!(redact_secrets_in_value(&input), input);
    }

    #[test]
    fn should_return_owned_text_only_when_something_was_redacted() {
        assert!(matches!(
            redact_secrets_in_text("plain text"),
            Cow::Borrowed(_)
        ));
        assert!(matches!(
            redact_secrets_in_text("token=abc123"),
            Cow::Owned(_)
        ));
    }

    #[test]
    fn should_detect_secret_when_text_would_be_redacted() {
        assert!(contains_secret(&format!("key={OPENAI_KEY}")));
        assert!(contains_secret("Authorization: Bearer abc.def.ghi"));
        assert!(contains_secret("AKIAIOSFODNN7EXAMPLE"));
        assert!(contains_secret("--password hunter2"));
        assert!(!contains_secret("the token bucket"));
        assert!(!contains_secret("Authorization: Bearer [REDACTED]"));
        assert!(!contains_secret("api_key=[REDACTED]"));
    }

    // --- 6. idempotence ---------------------------------------------------------------

    #[test]
    fn should_leave_output_unchanged_when_redacting_already_redacted_output() {
        let value = json!({
            "env": { "OPENAI_API_KEY": OPENAI_KEY },
            "cmd": format!(
                "curl -H 'Authorization: Bearer {LIVE_KEY}' https://api.example/v1 --password hunter2"
            ),
            "headers": { "Cookie": "a=1" }
        });
        let once = redact_secrets_in_value(&value);
        let twice = redact_secrets_in_value(&once);
        assert_eq!(once, twice);

        let text_once = text(&format!(
            "Authorization: Bearer {LIVE_KEY}; api_key={OPENAI_KEY}; https://u:p@h/ -----BEGIN RSA PRIVATE KEY-----\nabc\n-----END RSA PRIVATE KEY-----"
        ));
        assert_eq!(
            text_once,
            "Authorization: Bearer [REDACTED]; api_key=[REDACTED]; https://u:[REDACTED]@h/ [REDACTED]"
        );
        assert!(matches!(
            redact_secrets_in_text(&text_once),
            Cow::Borrowed(_)
        ));
        assert!(!contains_secret(&text_once));
    }

    // --- robustness ------------------------------------------------------------------

    #[test]
    fn should_treat_containers_as_opaque_text_when_nesting_exceeds_depth_cap() {
        let mut value = json!({ "api_key": OPENAI_KEY, "n": 1 });
        for _ in 0..80 {
            value = json!({ "a": value });
        }
        let out = redact_secrets_in_value(&value);
        let mut cursor = &out;
        for _ in 0..64 {
            cursor = &cursor["a"];
        }
        let opaque = cursor
            .as_str()
            .expect("container beyond the depth cap becomes an opaque string");
        assert!(!opaque.contains(OPENAI_KEY));
        assert!(opaque.contains("\"api_key\":\"[REDACTED]\""));
        assert!(opaque.contains("\"n\":1"));
        assert!(!out.to_string().contains(OPENAI_KEY));
        assert_eq!(redact_secrets_in_value(&out), out);
    }

    #[test]
    fn should_not_panic_when_input_is_odd_or_adversarial() {
        let long = "a".repeat(100_000);
        let samples: Vec<String> = vec![
            String::new(),
            "\"".into(),
            "'".into(),
            "\\".into(),
            "\\\"".into(),
            "{".into(),
            "[".into(),
            "://".into(),
            "https://user:pass@".into(),
            "https://:@".into(),
            "-----BEGIN ".into(),
            "-----BEGIN PRIVATE KEY-----".into(),
            "-----BEGIN PRIVATE KEY-----\n-----END ".into(),
            "sk-".into(),
            format!("sk-{long}"),
            "AKIA".into(),
            "AIza".into(),
            "ghp_".into(),
            "github_pat_".into(),
            "glpat-".into(),
            "xoxb-".into(),
            "eyJ".into(),
            "eyJ..".into(),
            "eyJa.b.".into(),
            "Bearer".into(),
            "Bearer ".into(),
            "Authorization:".into(),
            "Authorization: Bearer".into(),
            "api_key=".into(),
            "api_key=\"".into(),
            "api_key='".into(),
            "\"api_key\": \"".into(),
            "token: ".into(),
            "--token".into(),
            "--token -x".into(),
            "\u{0}\u{0}api_key=\u{0}".into(),
            "é".into(),
            "api_key=é".into(),
            "api_key=\"é".into(),
            "sk-ééééééééééééééééééé".into(),
            "🔑 api_key=🔑".into(),
            "ключ=значение".into(),
            long.clone(),
            format!("api_key={long}"),
            format!("api_key=\"{long}"),
            "[".repeat(300),
            "{\"a\":".repeat(300),
            format!("{}{}", "[".repeat(200), "]".repeat(200)),
            "!@#$%^&*()".into(),
            "-".repeat(1000),
            "\"".repeat(1000),
            ":".repeat(1000),
            "://".repeat(1000),
            "Bearer ".repeat(1000),
            "api_key=\\\" ".repeat(500),
            "token=$(".repeat(20_000),
            "token=<".repeat(20_000),
            "token={{".repeat(20_000),
            format!("{}{}", "$(".repeat(1_000), ")".repeat(1_000)),
            format!("token=$(echo {})", "$(".repeat(100)),
            "<".repeat(100_000),
            format!("<{long}>"),
            format!("{{{{{long}}}}}"),
            "{{ ".repeat(100),
            "$".into(),
            "${".into(),
            "$(".into(),
            "${{".into(),
            "%".into(),
            "%%".into(),
            "<>".into(),
            "{{}}".into(),
        ];
        for sample in &samples {
            let once = redact_secrets_in_text(sample).into_owned();
            let _ = contains_secret(sample);
            let twice = redact_secrets_in_text(&once).into_owned();
            assert_eq!(once, twice, "text idempotence for {sample:?}");

            let value = json!({ "s": sample, "arr": [sample, { "api_key": sample }] });
            let v_once = redact_secrets_in_value(&value);
            let v_twice = redact_secrets_in_value(&v_once);
            assert_eq!(v_once, v_twice, "value idempotence for {sample:?}");
        }
    }

    // --- deliberate boundaries (documented limitations) -------------------------------

    #[test]
    fn should_follow_documented_boundaries_when_inputs_are_ambiguous() {
        // Structural: a credential-named header keeps its scheme word, the
        // credential after it is replaced.
        let input = json!({ "headers": { "Authorization": "Bearer abc", "Accept": "*/*" } });
        let expected =
            json!({ "headers": { "Authorization": "Bearer [REDACTED]", "Accept": "*/*" } });
        assert_eq!(redact_secrets_in_value(&input), expected);

        // Fail closed: `<...>` needs an env-style NAME or placeholder wording
        // (`<your-…>`, `<insert …>`); a bare noun is redacted. Inside `{{ }}`
        // an identifier-like path is a template expression and is kept.
        let input = json!({ "api_key": "<api-key>", "token": "{{ vault_token }}" });
        let expected = json!({ "api_key": "[REDACTED]", "token": "{{ vault_token }}" });
        assert_eq!(redact_secrets_in_value(&input), expected);

        // Spec lists `pwd`, so the shell's PWD variable is (over-)redacted.
        assert_eq!(text("PWD=/home/user"), "PWD=[REDACTED]");
        // A path handed to a credential-named flag is redacted (over-redaction).
        assert_eq!(
            text("ssh --private-key ~/.ssh/id_rsa host"),
            "ssh --private-key [REDACTED] host"
        );
        // Bare `Bearer` followed by a base64-looking mixed-case word is treated as a token.
        assert_eq!(text("Bearer dXNlcjpwYXNz"), "Bearer [REDACTED]");

        // Not redacted on purpose: prose-style "key value" without a
        // separator, single-letter flags, and `sk-` glued to a hyphenated word.
        for sample in [
            "password hunter2",
            "mysql -p hunter2",
            "desk-sk-frontend-deployment-1234567890",
            "https://host/reset?keyword=abc123",
        ] {
            assert!(
                matches!(redact_secrets_in_text(sample), Cow::Borrowed(_)),
                "expected untouched: {sample:?}"
            );
        }
    }

    // --- GAP-2a: credential-named fields fail closed ----------------------------------

    #[test]
    fn should_redact_wrapped_value_when_placeholder_is_not_demonstrably_symbolic() {
        let input = json!({
            "api_key": "<actual-secret>",
            "password": "<p@ssw0rd!>",
            "token": "{{sk-live-abcdefghijklmnopqrstuvwxyz0123}}",
            "secret": "${sk-live-abcdefghijklmnopqrstuvwxyz0123}",
            "client_secret": "$(echo sk-live-abcdefghijklmnopqrstuvwxyz0123)",
            "auth_token": "$(TOKEN=abc123 ./mint)",
            "passwd": "$lowercase_var",
            "credential": "<dXNlcjpwYXNz>",
            "cookie": "sid=abc; Path=/",
            "refresh_token": "{{ 4f3c9a7b1d2e }}"
        });
        let expected = json!({
            "api_key": "[REDACTED]",
            "password": "[REDACTED]",
            "token": "[REDACTED]",
            "secret": "[REDACTED]",
            "client_secret": "[REDACTED]",
            "auth_token": "[REDACTED]",
            "passwd": "[REDACTED]",
            "credential": "[REDACTED]",
            "cookie": "[REDACTED]",
            "refresh_token": "[REDACTED]"
        });
        assert_eq!(redact_secrets_in_value(&input), expected);
    }

    #[test]
    fn should_keep_symbolic_reference_when_key_is_credential_named() {
        let input = json!({
            "api_key": "<YOUR_API_KEY>",
            "apiKey": "${OPENAI_API_KEY}",
            "token": "{{ secrets.GH_TOKEN }}",
            "access_token": "${{ secrets.GH_TOKEN }}",
            "secret": "$GITHUB_TOKEN",
            "password": "%DB_PASSWORD%",
            "passwd": "<your-api-key>",
            "client_secret": "<insert client secret here>",
            "secret_key": "<API-KEY>",
            "session_token": "<xxxx-xxxx>",
            "auth": "{{NAME}}",
            "id_token": "{{ .Values.apiKey }}",
            "credential": "$(cat ~/.config/openai/key.txt)",
            "private_key": "",
            "pwd": "***",
            "aws_secret_access_key": "[REDACTED]",
            "authorization": "Bearer ${TOKEN}"
        });
        assert_eq!(redact_secrets_in_value(&input), input);
    }

    #[test]
    fn should_keep_auth_scheme_when_credential_value_starts_with_scheme() {
        let input = json!({
            "auth": {
                "headers": {
                    "Authorization": "Bearer <eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.abc>"
                }
            }
        });
        let expected = json!({
            "auth": { "headers": { "Authorization": "Bearer [REDACTED]" } }
        });
        assert_eq!(redact_secrets_in_value(&input), expected);

        let input = json!({
            "headers": { "Authorization": "Basic dXNlcjpwYXNz", "Accept": "*/*" }
        });
        let expected = json!({
            "headers": { "Authorization": "Basic [REDACTED]", "Accept": "*/*" }
        });
        assert_eq!(redact_secrets_in_value(&input), expected);
    }

    #[test]
    fn should_redact_token_inside_wrapper_when_text_or_non_credential_field_wraps_it() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.abc";
        let cases: Vec<(String, &str)> = vec![
            (
                format!("--api-key '<{OPENAI_KEY}>'"),
                "--api-key '[REDACTED]'",
            ),
            (format!("--api-key <{OPENAI_KEY}>"), "--api-key [REDACTED]"),
            (format!("token=${{{OPENAI_KEY}}}"), "token=[REDACTED]"),
            (
                format!("note: <{OPENAI_KEY}> and {{{{{OPENAI_KEY}}}}}"),
                "note: <[REDACTED]> and {{[REDACTED]}}",
            ),
            (
                format!("Authorization: Bearer <{jwt}>"),
                "Authorization: Bearer [REDACTED]",
            ),
            (
                "[AKIAIOSFODNN7EXAMPLE] \"ghp_abcdefghijklmnopqrstuvwxyz0123456789\"".into(),
                "[[REDACTED]] \"[REDACTED]\"",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(text(&input), expected, "input {input:?}");
            assert!(contains_secret(&input), "input {input:?}");
        }

        let input = json!({
            "note": format!("<{OPENAI_KEY}>"),
            "template": format!("{{{{{OPENAI_KEY}}}}}")
        });
        let expected = json!({ "note": "<[REDACTED]>", "template": "{{[REDACTED]}}" });
        assert_eq!(redact_secrets_in_value(&input), expected);
    }

    #[test]
    fn should_keep_symbolic_reference_when_text_value_follows_credential_key() {
        for sample in [
            "api_key=$OPENAI_API_KEY",
            "--api-key '<your-api-key>'",
            "--api-key <YOUR_API_KEY>",
            "token: '{{ secrets.GH_TOKEN }}'",
            "token: {{ secrets.GH_TOKEN }}",
            "--password \"%DB_PASSWORD%\"",
            "Authorization: Bearer ${TOKEN}",
            "--token \"$(cat ~/.token)\"",
            "--token $(cat ~/.token)",
            "password: <redacted>",
            "secret: ***",
        ] {
            assert!(
                matches!(redact_secrets_in_text(sample), Cow::Borrowed(_)),
                "expected untouched: {sample:?}"
            );
            assert!(!contains_secret(sample), "false positive: {sample:?}");
        }
    }

    /// Bracketed unquoted values used to scan as an empty (exempt) value
    /// because `[`, `{` and `(` terminate a bare value.
    #[test]
    fn should_redact_bracketed_value_after_credential_key() {
        assert_eq!(text("token=[abc123]"), "token=[REDACTED]");
        assert_eq!(text("password={abc123}"), "password=[REDACTED]");
        assert_eq!(text("secret=(abc123)"), "secret=[REDACTED]");
        assert_eq!(
            text("api_key: [abc123] and more"),
            "api_key: [REDACTED] and more"
        );
    }

    /// `curl -u user:password` is the most common way a credential enters a
    /// shell command; the user half stays visible, the password is scrubbed.
    #[test]
    fn should_redact_basic_auth_after_curl_user_flag() {
        assert_eq!(
            text("curl -u alice:hunter2 https://api.example"),
            "curl -u alice:[REDACTED] https://api.example"
        );
        assert_eq!(
            text("curl --user 'alice:hunter2' https://api.example"),
            "curl --user 'alice:[REDACTED]' https://api.example"
        );
        assert_eq!(
            text("curl --proxy-user=alice:hunter2 https://api.example"),
            "curl --proxy-user=alice:[REDACTED] https://api.example"
        );
        // A bare user name carries no secret.
        assert_eq!(
            text("curl -u alice https://api.example"),
            "curl -u alice https://api.example"
        );
        assert!(contains_secret("curl -u alice:hunter2 https://api.example"));
    }

    /// Ordinary code and language literals under a credential-like name are
    /// not credentials: an identifier call, `None`, `nil`, `undefined`.
    #[test]
    fn should_keep_code_assignment_when_value_is_identifier_call_or_null_literal() {
        for sample in [
            "let token = tokenizer.next();",
            "self.password = None",
            "password = nil",
            "const secret = undefined;",
            "token = parse_token(line)",
        ] {
            assert!(
                matches!(redact_secrets_in_text(sample), Cow::Borrowed(_)),
                "expected untouched: {sample:?}"
            );
        }
    }

    #[test]
    fn should_redact_non_symbolic_text_value_when_it_follows_credential_key() {
        let cases: Vec<(String, &str)> = vec![
            (
                "--api-key '<actual-secret>'".into(),
                "--api-key '[REDACTED]'",
            ),
            ("password=$lowercase_var".into(), "password=[REDACTED]"),
            (
                "--token $(echo sk-live-abcdefghijklmnopqrstuvwxyz0123)".into(),
                "--token [REDACTED]",
            ),
            ("secret: <p@ssw0rd!>".into(), "secret: [REDACTED]"),
            (
                r#""credentials": "{\"user\":\"a\",\"password\":\"b\"}""#.into(),
                r#""credentials": "[REDACTED]""#,
            ),
            ("token: {{ 4f3c9a7b1d2e }}".into(), "token: [REDACTED]"),
        ];
        for (input, expected) in cases {
            assert_eq!(text(&input), expected, "input {input:?}");
        }
    }
}
