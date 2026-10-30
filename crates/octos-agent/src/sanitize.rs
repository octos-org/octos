//! Tool output sanitization.
//!
//! Strips base64 data URIs and credentials from tool
//! results before feeding them back to the LLM, reducing context waste
//! and preventing secret leakage.
//! Content hashes are ordinary artifact identifiers, not credentials. Their
//! length alone cannot justify rewriting paths, checksums, or validator evidence.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;

/// Base64 data URIs: `data:...;base64,<payload>`
static DATA_URI_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"data:[^;]{1,100};base64,[A-Za-z0-9+/=]{64,}").unwrap());

// ---------------------------------------------------------------------------
// Credential patterns
// ---------------------------------------------------------------------------

/// OpenAI API keys: `sk-` followed by 20+ alphanumeric/dash chars.
static OPENAI_KEY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"sk-[A-Za-z0-9_-]{20,}").unwrap());

/// Anthropic API keys: `sk-ant-` prefix.
static ANTHROPIC_KEY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"sk-ant-[A-Za-z0-9_-]{20,}").unwrap());

/// AWS access key IDs: `AKIA` followed by 16 uppercase alphanumeric chars.
static AWS_KEY_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"AKIA[0-9A-Z]{16}").unwrap());

/// GitHub tokens: `ghp_`, `gho_`, `ghs_`, `ghr_`, `github_pat_` prefixes.
static GITHUB_TOKEN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:ghp_|gho_|ghs_|ghr_|github_pat_)[A-Za-z0-9_]{20,}").unwrap());

/// GitLab personal access tokens: `glpat-` prefix.
static GITLAB_TOKEN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"glpat-[A-Za-z0-9_-]{20,}").unwrap());

/// Bearer tokens in HTTP headers.
static BEARER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Bearer\s+[A-Za-z0-9_.+/=-]{20,}").unwrap());

/// Generic secret assignments: `password|secret|token|api_key = "..."` or `= '...'`.
static SECRET_ASSIGN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)(?:password|secret|api_key|apikey|access_token|auth_token|private_key)(?:\\?["'])?\s*[=:]\s*(?:\\?["'])?[A-Za-z0-9_.+/=-]{8,}(?:\\?["'])?"#,
    )
    .unwrap()
});

/// Redact a credential match, preserving the first 4 visible characters for context.
fn redact_credential(m: &regex::Match<'_>) -> String {
    let text = m.as_str();
    let prefix: String = text.chars().take(4).collect();
    format!("{prefix}...[credential-redacted]")
}

/// Scrub known credential patterns from text.
fn scrub_credentials(input: &str) -> Cow<'_, str> {
    // Order matters: more specific patterns first to avoid partial matches.
    let result = ANTHROPIC_KEY_RE.replace_all(input, |caps: &regex::Captures<'_>| {
        redact_credential(&caps.get(0).unwrap())
    });
    let result = OPENAI_KEY_RE.replace_all(&result, |caps: &regex::Captures<'_>| {
        redact_credential(&caps.get(0).unwrap())
    });
    let result = AWS_KEY_RE.replace_all(&result, |caps: &regex::Captures<'_>| {
        redact_credential(&caps.get(0).unwrap())
    });
    let result = GITHUB_TOKEN_RE.replace_all(&result, |caps: &regex::Captures<'_>| {
        redact_credential(&caps.get(0).unwrap())
    });
    let result = GITLAB_TOKEN_RE.replace_all(&result, |caps: &regex::Captures<'_>| {
        redact_credential(&caps.get(0).unwrap())
    });
    let result = BEARER_RE.replace_all(&result, |caps: &regex::Captures<'_>| {
        redact_credential(&caps.get(0).unwrap())
    });
    let result = SECRET_ASSIGN_RE.replace_all(&result, |caps: &regex::Captures<'_>| {
        redact_credential(&caps.get(0).unwrap())
    });
    Cow::Owned(result.into_owned())
}

/// Sanitize tool output by stripping base64 data URIs,
/// credential patterns, and prompt injection attempts.
pub fn sanitize_tool_output(input: &str) -> String {
    let after_data_uri = DATA_URI_RE.replace_all(input, "[base64-data-redacted]");
    let after_creds = scrub_credentials(&after_data_uri);
    crate::prompt_guard::sanitize_injection(&after_creds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strips_data_uri() {
        let input = "img: data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg== done";
        let result = sanitize_tool_output(input);
        assert_eq!(result, "img: [base64-data-redacted] done");
    }

    #[test]
    fn test_preserves_artifact_paths_and_integrity_digests() {
        for length in [64, 128] {
            let hex = "a".repeat(length);
            for input in [
                format!("qa/{hex}/report.json"),
                format!("C:\\artifacts\\{hex}\\result.json"),
                format!("{hex}  build.tar.gz"),
                format!(r#"{{"sha256":"{hex}","artifact":"objects/{hex}"}}"#),
            ] {
                assert_eq!(sanitize_tool_output(&input), input);
            }
        }
    }

    #[test]
    fn test_hex_credentials_still_redacted_in_text_json_and_nested_json() {
        let hex = "a".repeat(64);
        for input in [
            format!("api_key={hex}"),
            format!("Authorization: Bearer {hex}"),
            format!(r#"{{"api_key":"{hex}"}}"#),
            serde_json::to_string(&format!(r#"{{"private_key":"{hex}"}}"#)).unwrap(),
        ] {
            let result = sanitize_tool_output(&input);
            assert!(result.contains("[credential-redacted]"), "{result}");
            assert!(!result.contains(&hex), "{result}");
        }
    }

    #[test]
    fn test_preserves_short_hex() {
        let input = "commit abc123def456 is good";
        let result = sanitize_tool_output(input);
        assert_eq!(result, input);
    }

    #[test]
    fn test_preserves_normal_text() {
        let input = "Hello world, this is normal output.";
        let result = sanitize_tool_output(input);
        assert_eq!(result, input);
    }

    #[test]
    fn test_multiple_replacements() {
        let hex = "f".repeat(64);
        let input = format!(
            "data:text/plain;base64,{} and secret={} end",
            "A".repeat(100),
            hex
        );
        let result = sanitize_tool_output(&input);
        assert!(result.contains("[base64-data-redacted]"));
        assert!(result.contains("[credential-redacted]"));
        assert!(!result.contains(&hex));
    }

    // -----------------------------------------------------------------------
    // Credential scrubbing tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_scrubs_openai_key() {
        let input = "OPENAI_API_KEY=sk-proj-abc123def456ghi789jklmnopqrst";
        let result = sanitize_tool_output(input);
        assert!(result.contains("[credential-redacted]"));
        assert!(result.contains("sk-p"));
        assert!(!result.contains("abc123def456"));
    }

    #[test]
    fn test_scrubs_anthropic_key() {
        let input = "key: sk-ant-api03-abcdefghijklmnopqrstuvwxyz123456";
        let result = sanitize_tool_output(input);
        assert!(result.contains("[credential-redacted]"));
        assert!(result.contains("sk-a"));
        assert!(!result.contains("abcdefghij"));
    }

    #[test]
    fn test_scrubs_aws_access_key() {
        let input = "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE";
        let result = sanitize_tool_output(input);
        assert!(result.contains("[credential-redacted]"));
        assert!(result.contains("AKIA"));
        assert!(!result.contains("IOSFODNN7EXAMPLE"));
    }

    #[test]
    fn test_scrubs_github_token() {
        let input = "token: ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ12345678";
        let result = sanitize_tool_output(input);
        assert!(result.contains("[credential-redacted]"));
        assert!(result.contains("ghp_"));
        assert!(!result.contains("aBcDeFgHiJkL"));
    }

    #[test]
    fn test_scrubs_github_pat() {
        let input = "GITHUB_TOKEN=github_pat_abcdefghij1234567890abcdef";
        let result = sanitize_tool_output(input);
        assert!(result.contains("[credential-redacted]"));
        assert!(!result.contains("abcdefghij123"));
    }

    #[test]
    fn test_scrubs_gitlab_token() {
        let input = "GITLAB_TOKEN=glpat-xxxxxxxxxxxxxxxxxxxx";
        let result = sanitize_tool_output(input);
        assert!(result.contains("[credential-redacted]"));
        assert!(!result.contains("xxxxxxxxxxxxxxxxxxxx"));
    }

    #[test]
    fn test_scrubs_bearer_token() {
        let input = "Authorization: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.abcdef";
        let result = sanitize_tool_output(input);
        assert!(result.contains("[credential-redacted]"));
        assert!(!result.contains("eyJhbGciOiJ"));
    }

    #[test]
    fn test_scrubs_password_assignment() {
        let input = r#"password = "SuperSecret123!abc""#;
        let result = sanitize_tool_output(input);
        assert!(result.contains("[credential-redacted]"));
        assert!(!result.contains("SuperSecret123"));
    }

    #[test]
    fn test_scrubs_api_key_assignment() {
        let input = "api_key: abcdefghijklmnopqrstuvwxyz123456";
        let result = sanitize_tool_output(input);
        assert!(result.contains("[credential-redacted]"));
    }

    #[test]
    fn test_preserves_normal_env_vars() {
        let input = "PATH=/usr/local/bin:/usr/bin\nHOME=/home/user";
        let result = sanitize_tool_output(input);
        assert_eq!(result, input);
    }

    #[test]
    fn test_scrubs_multiple_credentials() {
        let input =
            "keys: sk-proj-abcdefghijklmnopqrstu and ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ12345678";
        let result = sanitize_tool_output(input);
        assert_eq!(
            result.matches("[credential-redacted]").count(),
            2,
            "should redact both credentials: {result}"
        );
    }

    #[test]
    fn test_preserves_short_tokens() {
        // Short strings that look like prefixes but are too short
        let input = "sk-abc";
        let result = sanitize_tool_output(input);
        assert_eq!(result, input);
    }
}
