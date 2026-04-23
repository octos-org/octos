//! Harness starter: coding-assistant diff-artifact custom app.
//!
//! Declares `primary = "patches/*.diff"` and a `propose_patch` spawn-only
//! tool that renders a unified-diff stub. Also declares a `preview`
//! artifact (`patches/*.files.txt`) listing the files that the diff
//! touches.
//!
//! The "diff" content is deliberately minimal — this starter focuses on the
//! harness contract shape, not on real code generation. Swap in a real
//! LLM-driven patch synthesis when adapting.
//!
//! See `docs/OCTOS_HARNESS_DEVELOPER_GUIDE.md`.

#![deny(unsafe_code)]

use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct ProposePatchInput {
    /// Short human-readable title for the patch; used to derive the
    /// filename.
    pub title: String,
    /// Hunks that make up the patch. Each hunk declares the file it
    /// targets and the replacement content. For the starter, we generate
    /// a full-file replacement diff per hunk.
    pub hunks: Vec<PatchHunk>,
}

#[derive(Debug, Deserialize)]
pub struct PatchHunk {
    pub file: String,
    pub new_content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposePatchOutput {
    pub diff_path: PathBuf,
    pub preview_path: PathBuf,
    pub changed_files: Vec<String>,
}

/// Render a patch into `patches/<slug>.diff` plus a preview file list at
/// `patches/<slug>.files.txt`.
pub fn propose_patch(
    workspace_root: &Path,
    input: &ProposePatchInput,
) -> Result<ProposePatchOutput> {
    if input.hunks.is_empty() {
        eyre::bail!("propose_patch requires at least one hunk");
    }
    let dir = workspace_root.join("patches");
    std::fs::create_dir_all(&dir)
        .wrap_err_with(|| format!("create patches dir failed: {}", dir.display()))?;

    let slug = slugify(&input.title);
    let diff_relative = Path::new("patches").join(format!("{slug}.diff"));
    let preview_relative = Path::new("patches").join(format!("{slug}.files.txt"));

    let diff_full = workspace_root.join(&diff_relative);
    let preview_full = workspace_root.join(&preview_relative);

    let mut changed_files = Vec::with_capacity(input.hunks.len());
    let mut diff = String::new();
    for hunk in &input.hunks {
        changed_files.push(hunk.file.clone());
        render_hunk(&mut diff, &hunk.file, &hunk.new_content);
    }

    std::fs::write(&diff_full, diff.as_bytes())
        .wrap_err_with(|| format!("write diff failed: {}", diff_full.display()))?;

    let preview = changed_files.join("\n") + "\n";
    std::fs::write(&preview_full, preview.as_bytes())
        .wrap_err_with(|| format!("write preview failed: {}", preview_full.display()))?;

    Ok(ProposePatchOutput {
        diff_path: diff_relative,
        preview_path: preview_relative,
        changed_files,
    })
}

/// Append a unified-diff-style "replace whole file" hunk. The hunk is a
/// simplified full-file replacement, not a minimized unified diff — real
/// coding assistants should use `diff` or `similar` to compute minimal
/// hunks. We keep it simple so the starter stays small.
fn render_hunk(out: &mut String, file: &str, new_content: &str) {
    out.push_str(&format!("--- a/{file}\n"));
    out.push_str(&format!("+++ b/{file}\n"));
    let line_count = new_content.lines().count().max(1);
    out.push_str(&format!("@@ -0,0 +1,{line_count} @@\n"));
    for line in new_content.lines() {
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }
    if !new_content.ends_with('\n') {
        // Trailing newline to keep readers happy.
        out.push('\n');
    }
}

pub fn slugify(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut last_dash = false;
    for c in label.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "patch".to_string()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_reject_empty_hunks() {
        let tmp = tempfile::tempdir().unwrap();
        let input = ProposePatchInput {
            title: "empty".into(),
            hunks: Vec::new(),
        };
        let err = propose_patch(tmp.path(), &input).unwrap_err();
        assert!(err.to_string().contains("at least one hunk"));
    }

    #[test]
    fn should_render_unified_diff_and_preview() {
        let tmp = tempfile::tempdir().unwrap();
        let input = ProposePatchInput {
            title: "Fix typo".into(),
            hunks: vec![PatchHunk {
                file: "src/lib.rs".into(),
                new_content: "pub fn hello() -> &'static str {\n    \"hi\"\n}\n".into(),
            }],
        };
        let out = propose_patch(tmp.path(), &input).unwrap();

        assert_eq!(out.diff_path, Path::new("patches/fix-typo.diff"));
        assert_eq!(out.preview_path, Path::new("patches/fix-typo.files.txt"));
        assert_eq!(out.changed_files, vec!["src/lib.rs"]);

        let diff = std::fs::read_to_string(tmp.path().join(&out.diff_path)).unwrap();
        assert!(diff.contains("--- a/src/lib.rs"));
        assert!(diff.contains("+++ b/src/lib.rs"));
        assert!(diff.contains("+pub fn hello()"));

        let preview = std::fs::read_to_string(tmp.path().join(&out.preview_path)).unwrap();
        assert_eq!(preview.trim(), "src/lib.rs");
    }

    #[test]
    fn should_emit_hunk_for_each_file() {
        let tmp = tempfile::tempdir().unwrap();
        let input = ProposePatchInput {
            title: "multi".into(),
            hunks: vec![
                PatchHunk {
                    file: "a.rs".into(),
                    new_content: "a\n".into(),
                },
                PatchHunk {
                    file: "b.rs".into(),
                    new_content: "b\n".into(),
                },
            ],
        };
        let out = propose_patch(tmp.path(), &input).unwrap();
        assert_eq!(out.changed_files, vec!["a.rs", "b.rs"]);
        let diff = std::fs::read_to_string(tmp.path().join(&out.diff_path)).unwrap();
        assert_eq!(diff.matches("--- a/").count(), 2);
    }
}
