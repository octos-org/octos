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

fn redact_line(line: &str) -> String {
    let mut result = String::with_capacity(line.len());
    let mut tokens = line.split(' ').peekable();
    // Set when the NEXT token is a value to mask: after "Bearer", or after
    // a sensitive key with the value in the following token ("password: x").
    let mut value_pending = false;
    while let Some(token) = tokens.next() {
        let mut replaced: Option<String> = None;
        let bare = token.trim_end_matches(['\n', '\r', ',', ';', '"', '\'', ')']);

        if value_pending && looks_tokenish(bare) {
            replaced = Some(token.replace(bare, "[redacted]"));
        }
        value_pending = token.eq_ignore_ascii_case("bearer");

        if replaced.is_none() {
            if has_secret_prefix(bare) {
                replaced = Some(token.replace(bare, "[redacted]"));
            } else if let Some((key, value)) = split_assignment(bare) {
                if key_is_sensitive(key) {
                    if looks_tokenish(value) {
                        replaced = Some(token.replace(value, "[redacted]"));
                    } else if value.is_empty() {
                        // "password:" / "token=" with the value as the
                        // next token.
                        value_pending = true;
                    }
                }
            } else if is_long_token_run(bare) {
                replaced = Some(token.replace(bare, "[redacted]"));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_redact_prefixed_keys_when_present() {
        let out = redact_secrets("my key is sk-abc123def456ghi789 ok");
        assert!(!out.contains("sk-abc123"));
        assert!(out.contains("[redacted]"));
    }

    #[test]
    fn should_redact_assignments_when_key_sensitive() {
        let out = redact_secrets("export OPENAI_API_KEY=abcd1234efgh5678");
        assert!(!out.contains("abcd1234efgh5678"));
        let out2 = redact_secrets("password: hunter2hunter2");
        assert!(!out2.contains("hunter2hunter2"));
    }

    #[test]
    fn should_redact_bearer_tokens_when_present() {
        let out = redact_secrets("Authorization: Bearer eyJhbGciOiJIUzI1NiJ9");
        assert!(!out.contains("eyJhbGciOiJIUzI1NiJ9"));
    }

    #[test]
    fn should_redact_long_hex_runs_when_present() {
        let out = redact_secrets("digest 3f7a9c2e5b8d1f4a6c0e2b5d8f1a3c6e9b2d5f8a");
        assert!(!out.contains("3f7a9c2e5b8d1f4a"));
    }

    #[test]
    fn should_keep_ordinary_prose_when_no_secrets() {
        let text = "用户偏好深色模式 and likes concise replies. Meeting at 15:30 tomorrow.";
        assert_eq!(redact_secrets(text), text);
    }

    #[test]
    fn should_keep_long_words_without_digits() {
        // Pure-alpha long words (e.g. German compounds) are not keys.
        let text = "Donaudampfschifffahrtsgesellschaftskapitaen sailed";
        assert_eq!(redact_secrets(text), text);
    }
}
