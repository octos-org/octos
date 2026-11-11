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
/// 1. Folds repeated path separators and resolves literal `.`/`..` segments
///    in-place. `..` segments that escape the root are dropped, leaving the
///    remaining tail. Absolute paths keep their leading `/`.
/// 2. Strips Unicode categories that the UI cannot safely render —
///    bidi overrides, zero-width characters, BOM, and other ambiguity controls.
///    Normalization deliberately does not run again: a control character that
///    was hiding inside a segment must not create a new traversal collapse.
///
/// The output is never longer than the input. If sanitising produces an
/// empty string (e.g. the input was only zero-width characters) the
/// returned string is empty — callers can fall back to a placeholder.
pub fn sanitize_display_path(input: &str) -> String {
    let absolute = input.starts_with('/');
    let mut segments: Vec<&str> = Vec::new();
    for segment in input.split('/') {
        match segment {
            "" | "." => continue,
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    let normalized = segments.join("/");
    let stripped: String = normalized
        .chars()
        .filter(|ch| !is_dangerous_display_char(*ch))
        .collect();
    if absolute {
        format!("/{stripped}")
    } else {
        stripped
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_display_path_does_not_fold_dotdot_formed_by_control_removal() {
        for control in ['\n', '\r', '\x1b'] {
            let input = format!("sub/..{control}/label.txt");
            let output = sanitize_display_path(&input);
            assert_eq!(
                output, "sub/../label.txt",
                "{control:?} must not turn into a new traversal segment before normalization"
            );
        }

        let c1 = sanitize_display_path("sub/..\u{85}/label.txt");
        assert_ne!(c1, "label.txt");
        assert!(c1.contains(".."));

        let fake_absolute = sanitize_display_path("..\n/..\n/..\n/..\n/..\n/..\n/etc/passwd");
        assert_eq!(fake_absolute, "../../../../../../etc/passwd");
        assert_ne!(fake_absolute, "/etc/passwd");
    }

    #[test]
    fn sanitize_display_path_preserves_spaces_and_utf8() {
        assert_eq!(sanitize_display_path("with space.txt"), "with space.txt");
        assert_eq!(sanitize_display_path("目录/文件.rs"), "目录/文件.rs");
        assert_eq!(sanitize_display_path("src/main.rs"), "src/main.rs");
    }

    #[test]
    fn sanitize_display_path_combines_traversal_and_unicode() {
        // Literal `..` is folded before the bidi control is removed. The
        // control-bearing first segment is not collapsed a second time.
        let combined = "..\u{202E}/secret/../etc/passwd";
        let sanitised = sanitize_display_path(combined);
        assert!(!sanitised.contains('\u{202E}'));
        assert_eq!(sanitised, "../etc/passwd");
    }
}
