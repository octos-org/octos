//! Smoke test for `harness-starter-report`.
//!
//! Matches the 4-part acceptance spec in
//! `docs/OCTOS_HARNESS_DEVELOPER_GUIDE.md` Part 2 Step 5.

use std::path::{Path, PathBuf};

use harness_starter_report::{GenerateReportInput, generate_report};
use octos_agent::workspace_policy::WorkspacePolicy;

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn tool_produces_primary_markdown_artifact() {
    let tmp = tempfile::tempdir().unwrap();
    let input = GenerateReportInput {
        topic: "Report Starter Smoke".into(),
        body: None,
    };
    let out = generate_report(tmp.path(), &input).unwrap();

    let full = tmp.path().join(&out.artifact_path);
    assert!(full.exists());
    let body = std::fs::read_to_string(&full).unwrap();
    assert!(body.starts_with("# Report Starter Smoke"));
    // The policy requires at least 256 bytes.
    assert!(body.len() >= 256, "body must satisfy file_size_min:256");

    // Path matches `reports/*.md`.
    let raw = std::fs::read_to_string(crate_root().join("workspace-policy.toml")).unwrap();
    let policy: WorkspacePolicy = toml::from_str(&raw).unwrap();
    let pattern = policy.artifacts.entries.get("primary").unwrap();
    assert!(
        path_matches_glob(&out.artifact_path, pattern),
        "produced path must match primary glob"
    );
}

fn path_matches_glob(path: &Path, pattern: &str) -> bool {
    // Normalize `\` -> `/` so the `/`-based policy globs match on Windows,
    // where `to_string_lossy()` renders backslash separators. The real runtime
    // globs the filesystem via the `glob` crate (separator-agnostic); this
    // helper only mirrors that at the string level for the smoke assertion.
    let path_str = path.to_string_lossy().replace('\\', "/");
    let (prefix, rest) = match pattern.split_once('*') {
        Some(pair) => pair,
        None => return path_str == pattern,
    };
    if !path_str.starts_with(prefix) {
        return false;
    }
    let after = &path_str[prefix.len()..];
    after.ends_with(rest)
}
