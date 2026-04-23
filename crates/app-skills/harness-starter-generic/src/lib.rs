//! Harness starter: minimal generic single-artifact app.
//!
//! Declares a single `primary` artifact and a `produce_artifact` spawn-only
//! tool. The tool writes a deterministic text file under `output/` so the
//! workspace contract can resolve it.
//!
//! This starter is the minimum legal shape for a harnessed custom app. Copy
//! it and rename when you want to build something more interesting.
//!
//! See `docs/OCTOS_HARNESS_DEVELOPER_GUIDE.md`.

#![deny(unsafe_code)]

use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr};
use serde::Deserialize;

/// Input accepted by the `produce_artifact` tool.
#[derive(Debug, Deserialize)]
pub struct ProduceArtifactInput {
    /// Required free-text label recorded inside the artifact.
    pub label: String,
}

/// Output of a successful `produce_artifact` run.
///
/// The runtime-resolved artifact path is a stable relative path: this lets
/// the workspace policy's `primary = "output/artifact-*.txt"` glob match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProduceArtifactOutput {
    pub artifact_path: PathBuf,
}

/// Core of the `produce_artifact` tool, factored out for testability.
///
/// Writes a deterministic text file under `<workspace_root>/output/` and
/// returns its relative path. The caller is responsible for wrapping this
/// in the plugin stdout protocol.
pub fn produce_artifact(
    workspace_root: &Path,
    input: &ProduceArtifactInput,
) -> Result<ProduceArtifactOutput> {
    let output_dir = workspace_root.join("output");
    std::fs::create_dir_all(&output_dir)
        .wrap_err_with(|| format!("create output dir failed: {}", output_dir.display()))?;

    let slug = slugify(&input.label);
    let relative = Path::new("output").join(format!("artifact-{slug}.txt"));
    let full = workspace_root.join(&relative);
    let contents = format!("harness-starter-generic\nlabel: {}\n", input.label);
    std::fs::write(&full, contents.as_bytes())
        .wrap_err_with(|| format!("write artifact failed: {}", full.display()))?;

    Ok(ProduceArtifactOutput {
        artifact_path: relative,
    })
}

/// Slugify a label to a safe filename fragment.
///
/// Keeps ASCII alphanumerics, collapses other characters into `-`. The
/// output is never empty so the resulting filename is always unique per
/// non-empty label.
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
        "artifact".to_string()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_slugify_label_to_kebab_ascii() {
        assert_eq!(slugify("Hello World"), "hello-world");
        assert_eq!(slugify("A/B_c"), "a-b-c");
        assert_eq!(slugify(""), "artifact");
        assert_eq!(slugify("!!!"), "artifact");
    }

    #[test]
    fn should_produce_artifact_at_documented_path() {
        let tmp = tempfile::tempdir().unwrap();
        let input = ProduceArtifactInput {
            label: "weekly report".into(),
        };
        let out = produce_artifact(tmp.path(), &input).unwrap();

        assert_eq!(
            out.artifact_path,
            Path::new("output/artifact-weekly-report.txt")
        );
        let full = tmp.path().join(&out.artifact_path);
        assert!(full.exists(), "artifact file must exist");
        let body = std::fs::read_to_string(&full).unwrap();
        assert!(body.contains("weekly report"));
    }
}
