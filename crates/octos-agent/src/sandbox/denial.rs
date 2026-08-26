//! Recognising sandbox denials in command output.
//!
//! A confined command can fail in a way the tool layer never sees: the wrapper
//! shell exits 0 while a child deep inside the pipeline was refused by the
//! kernel and printed `ls: ../octos: Operation not permitted` to stderr. The
//! exit status says success, so nothing upstream knows a policy boundary was
//! hit, and the model is left to guess — in practice it narrates the denial
//! back to the user and gives up.
//!
//! Detection is textual because that is the only signal available: seatbelt and
//! Landlock report denials to the *child* as a plain errno, and the child's own
//! error message is all that reaches us. That makes this a heuristic, and it is
//! deliberately only consulted when a real backend is active ([`Sandbox::is_noop`]
//! is false) — under no confinement an EPERM is a genuine filesystem error, not
//! a policy decision, and prompting for it would be noise.
//!
//! [`Sandbox::is_noop`]: super::Sandbox::is_noop

/// Signatures a confined child prints when the kernel refuses it.
///
/// EPERM (`Operation not permitted`) is what macOS seatbelt returns. EACCES
/// (`Permission denied`) is what Landlock and bwrap return, and is also what an
/// ordinary unreadable file returns — the `is_noop` gate above is what keeps
/// the second case from producing spurious prompts.
const DENIAL_SIGNATURES: &[&str] = &[
    "Operation not permitted",
    "operation not permitted",
    "Permission denied",
    "permission denied",
    "(os error 1)",
    "(os error 13)",
];

/// One line of output that looks like a sandbox denial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxDenial {
    /// The offending line, trimmed and length-capped.
    pub line: String,
    /// Path the line blamed, when one could be recovered.
    pub path: Option<String>,
}

/// Longest denial line we echo into an approval prompt. Long enough for a
/// realistic `prog: /some/path: Operation not permitted`, short enough that a
/// pathological line cannot dominate the dialog.
const MAX_LINE: usize = 200;

/// Scan command output for lines that look like sandbox denials.
///
/// Returns at most `limit` distinct denials, in the order they appeared. Callers
/// are expected to have checked that a real sandbox backend is active.
pub fn detect_sandbox_denials(output: &str, limit: usize) -> Vec<SandboxDenial> {
    let mut found: Vec<SandboxDenial> = Vec::new();

    for raw in output.lines() {
        if found.len() >= limit {
            break;
        }
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if !DENIAL_SIGNATURES.iter().any(|sig| line.contains(sig)) {
            continue;
        }

        let mut line = line.to_owned();
        if line.chars().count() > MAX_LINE {
            line = line.chars().take(MAX_LINE).collect::<String>() + "…";
        }
        // Same path refused by several commands in one pipeline is one fact to
        // show the user, not three.
        let path = extract_path(&line);
        if found
            .iter()
            .any(|seen| seen.line == line || (path.is_some() && seen.path == path))
        {
            continue;
        }
        found.push(SandboxDenial { line, path });
    }

    found
}

/// Recover the path a denial line blamed.
///
/// Handles the common `prog: /path: Operation not permitted` shape, then falls
/// back to the first path-looking whitespace-delimited token on the line.
fn extract_path(line: &str) -> Option<String> {
    // `ls: ../octos: Operation not permitted` -> the middle colon-field.
    let fields: Vec<&str> = line.split(':').map(str::trim).collect();
    if fields.len() >= 3 {
        for field in &fields[1..fields.len() - 1] {
            if looks_like_path(field) {
                return Some((*field).to_owned());
            }
        }
    }

    line.split_whitespace()
        .map(|token| token.trim_matches(|c| matches!(c, '"' | '\'' | ',' | ':' | '(' | ')')))
        .find(|token| looks_like_path(token))
        .map(str::to_owned)
}

/// Whether a token is plausibly a filesystem path rather than prose.
fn looks_like_path(token: &str) -> bool {
    if token.is_empty() || token.contains(char::is_whitespace) {
        return false;
    }
    token.starts_with('/')
        || token.starts_with("./")
        || token.starts_with("../")
        || token.starts_with("~/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_seatbelt_denial_and_blamed_path() {
        let out = "fatal: Not a valid object name 9c18efd\nls: ../octos: Operation not permitted\nExit code: 0";
        let found = detect_sandbox_denials(out, 8);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path.as_deref(), Some("../octos"));
        assert!(found[0].line.contains("Operation not permitted"));
    }

    #[test]
    fn finds_absolute_path_denial() {
        let found = detect_sandbox_denials("sh: /tmp/rust195.txt: Operation not permitted", 8);
        assert_eq!(found[0].path.as_deref(), Some("/tmp/rust195.txt"));
    }

    #[test]
    fn finds_rust_io_error_form() {
        // `std::io::Error` renders EPERM as `(os error 1)` with no colon-path.
        let found = detect_sandbox_denials("Error: failed to read settings.toml (os error 1)", 8);
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn clean_output_yields_nothing() {
        let found = detect_sandbox_denials("all tests passed\nExit code: 0", 8);
        assert!(found.is_empty());
    }

    #[test]
    fn same_path_reported_twice_is_one_denial() {
        let out = "ls: /a/b: Operation not permitted\nstat: /a/b: Operation not permitted";
        assert_eq!(detect_sandbox_denials(out, 8).len(), 1);
    }

    #[test]
    fn respects_the_limit() {
        let out = "ls: /a: Operation not permitted\nls: /b: Operation not permitted\nls: /c: Operation not permitted";
        assert_eq!(detect_sandbox_denials(out, 2).len(), 2);
    }

    #[test]
    fn overlong_line_is_capped() {
        let out = format!("ls: /{}: Operation not permitted", "x".repeat(500));
        let found = detect_sandbox_denials(&out, 8);
        assert!(found[0].line.chars().count() <= MAX_LINE + 1);
    }

    #[test]
    fn prose_without_a_path_still_reports_the_line() {
        let found = detect_sandbox_denials("bind: Operation not permitted", 8);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, None);
    }
}
