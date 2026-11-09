//! Secret-shaped string redaction for extraction inputs.
//!
//! Session transcripts leave the process when the extraction model is
//! remote, so anything that looks like a credential is masked BEFORE the
//! call. Heuristic by design: prefer over-masking noise to leaking a key.

/// Redact secret-shaped substrings, replacing each with `[redacted]`.
///
/// Catches: common key prefixes (`sk-`, `ghp_`, `xoxb-`, `AKIA…`),
/// `Bearer <token>`, `KEY=value` / `TOKEN: value` assignments, and long
/// unbroken base64/hex runs (32+ chars with at least one digit).
pub(crate) fn redact_secrets(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        out.push_str(&redact_line(line));
    }
    out
}

/// Strip surrounding quote characters so `KEY="value"` and `'value'`
/// expose the inner token for both the tokenish check and the replace.
fn unquote(value: &str) -> &str {
    value.trim_matches(['"', '\''])
}

fn redact_line(line: &str) -> String {
    let mut result = String::with_capacity(line.len());
    let mut tokens = line.split(' ').peekable();
    // Set when the NEXT token is a value to mask: after "Bearer", after a
    // sensitive "password:" with the value in the following token, or
    // after a spaced separator ("PASSWORD = hunter2").
    let mut value_pending = false;
    // Set right after a bare sensitive key ("PASSWORD"); a following
    // "="/":" token promotes it to value_pending.
    let mut sep_pending = false;
    while let Some(token) = tokens.next() {
        // Runs of spaces produce empty tokens; they carry no content and
        // must NOT reset the pending-assignment state ("PASSWORD  =  x").
        if token.is_empty() {
            if tokens.peek().is_some() {
                result.push(' ');
            }
            continue;
        }
        let mut replaced: Option<String> = None;
        let bare = token.trim_end_matches(['\n', '\r', ',', ';', '"', '\'', ')']);
        let inner = unquote(bare);

        if value_pending && looks_tokenish(inner) {
            replaced = Some(token.replace(inner, "[redacted]"));
        }
        value_pending = token.eq_ignore_ascii_case("bearer");

        if sep_pending {
            sep_pending = false;
            if bare == "=" || bare == ":" {
                // Spaced assignment: the NEXT token is the value.
                value_pending = true;
            } else if let Some(rest) = bare.strip_prefix(['=', ':']) {
                // "PASSWORD =hunter2" — value glued to the separator.
                let rest = unquote(rest);
                if replaced.is_none() && looks_tokenish(rest) {
                    replaced = Some(token.replace(rest, "[redacted]"));
                }
            }
        }

        if replaced.is_none() {
            if has_secret_prefix(inner) {
                replaced = Some(token.replace(inner, "[redacted]"));
            } else if let Some((key, value)) = split_assignment(bare) {
                if key_is_sensitive(key) {
                    let value = unquote(value);
                    if looks_tokenish(value) {
                        replaced = Some(token.replace(value, "[redacted]"));
                    } else if value.is_empty() {
                        // "password:" / "token=" with the value as the
                        // next token.
                        value_pending = true;
                    }
                }
            } else if key_is_sensitive(bare) && !bare.is_empty() {
                // Bare sensitive key: watch for a spaced "=" / ":" next.
                sep_pending = true;
            } else if is_long_token_run(inner) {
                replaced = Some(token.replace(inner, "[redacted]"));
            }
        }

        result.push_str(&replaced.unwrap_or_else(|| token.to_string()));
        if tokens.peek().is_some() {
            result.push(' ');
        }
    }
    result
}

fn has_secret_prefix(token: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "sk-",
        "sk_live_",
        "sk_test_",
        "ghp_",
        "gho_",
        "github_pat_",
        "xoxb-",
        "xoxp-",
        "AKIA",
        "ASIA",
        "ya29.",
        "AIza",
    ];
    PREFIXES.iter().any(|p| token.starts_with(p)) && token.len() >= 12
}

fn split_assignment(token: &str) -> Option<(&str, &str)> {
    let idx = token.find(['=', ':'])?;
    let (key, rest) = token.split_at(idx);
    let value = rest[1..].trim();
    if key.is_empty() {
        return None;
    }
    // An empty value is meaningful to the caller: "password:" means the
    // value rides in the NEXT whitespace token.
    Some((key, value))
}

fn key_is_sensitive(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    [
        "key",
        "token",
        "secret",
        "password",
        "passwd",
        "credential",
        "auth",
    ]
    .iter()
    .any(|s| k.contains(s))
}

fn looks_tokenish(value: &str) -> bool {
    value.len() >= 8
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./+=".contains(c))
}

/// 32+ chars of unbroken base64/hex alphabet containing at least one digit
/// — the shape of raw keys, signatures, and session cookies.
fn is_long_token_run(token: &str) -> bool {
    token.len() >= 32
        && token.chars().any(|c| c.is_ascii_digit())
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "+/=_-".contains(c))
}
