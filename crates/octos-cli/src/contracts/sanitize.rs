//! Path sanitisation for display strings embedded in approval cards and
//! diff previews.
//!
//! Tools push `path`-shaped strings to the UI through progress events
//! (`UiFileMutationNotice.path`, `ApprovalCommandDetails.cwd`). Those values
//! later end up inside human-rendered `title`/`body` strings or inside
//! `DiffPreview.title` / `DiffPreviewFile.path`. Without sanitisation, a
//! malicious tool can spoof a path with:
//!
//! * `../` traversal sequences pointing outside the workspace
//!   (`../../etc/passwd`).
//! * Right-to-left override (U+202E) and other Unicode bidi controls that
//!   reverse rendered character order so `evil.txt\u{202E}cod.exe` looks
//!   like `evil.txtexe.doc`.
//! * Zero-width characters (U+200B–U+200F, U+FEFF) that hide segments and
//!   make two visually identical paths compare as different strings.
//!
//! The helper is conservative: it only rewrites **display strings**, never
//! filesystem paths used for IO. Callers that touch the filesystem must
//! continue to canonicalise via `octos_agent::tools::resolve_path`.

/// Sanitise a path-shaped string for embedding in a human-readable display
/// field (`title`, `body`, `DiffPreview.title`, `DiffPreviewFile.path`,
/// `ApprovalCommandDetails.cwd`).
///
/// The function:
/// 1. Strips Unicode categories that the UI cannot safely render —
///    bidi overrides, zero-width characters, BOM, and other ambiguity
///    controls (see [`is_dangerous_display_char`]).
/// 2. Folds repeated path separators and resolves `.`/`..` segments
///    in-place. `..` segments that escape the root are dropped, leaving
///    the remaining tail. Absolute paths keep their leading `/`.
///
/// The output is never longer than the input. If sanitising produces an
/// empty string (e.g. the input was only zero-width characters) the
/// returned string is empty — callers can fall back to a placeholder.
pub fn sanitize_display_path(input: &str) -> String {
    let stripped: String = input
        .chars()
        .filter(|ch| !is_dangerous_display_char(*ch))
        .collect();

    let absolute = stripped.starts_with('/');
    let mut segments: Vec<&str> = Vec::new();
    for segment in stripped.split('/') {
        match segment {
            "" | "." => continue,
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    let joined = segments.join("/");
    if absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

/// Returns `true` for characters that must be stripped before a path is
/// displayed in approval/diff text.
///
/// Categories covered:
///
/// * `U+202A`–`U+202E` and `U+2066`–`U+2069`: bidi overrides/isolates.
/// * `U+200B`–`U+200F`: zero-width spaces, joiners, LRM/RLM marks.
/// * `U+FEFF`: byte-order mark / zero-width no-break space.
/// * ASCII control codes (`< 0x20`) other than tab — these don't make
///   sense in a rendered path.
fn is_dangerous_display_char(ch: char) -> bool {
    let code = ch as u32;
    matches!(
        code,
        0x202A..=0x202E
            | 0x2066..=0x2069
            | 0x200B..=0x200F
            | 0xFEFF
    ) || (code < 0x20 && ch != '\t')
}
