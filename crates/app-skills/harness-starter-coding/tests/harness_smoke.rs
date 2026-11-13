//! Smoke test for `harness-starter-coding`.

use std::path::{Path, PathBuf};

use harness_starter_coding::{PatchHunk, ProposePatchInput, propose_patch};
use octos_agent::workspace_policy::WorkspacePolicy;

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn tool_produces_diff_and_preview_artifacts() {
    let tmp = tempfile::tempdir().unwrap();
    let input = ProposePatchInput {
        title: "coding smoke".into(),
        hunks: vec![PatchHunk {
            file: "src/main.rs".into(),
            new_content: "fn main() {\n    println!(\"hello harness\");\n}\n".into(),
        }],
    };
    let out = propose_patch(tmp.path(), &input).unwrap();

    let diff_full = tmp.path().join(&out.diff_path);
    let preview_full = tmp.path().join(&out.preview_path);
    assert!(diff_full.exists());
    assert!(preview_full.exists());

    let diff_bytes = std::fs::metadata(&diff_full).unwrap().len();
    assert!(diff_bytes >= 64, "diff must satisfy file_size_min:64");

    let preview_body = std::fs::read_to_string(&preview_full).unwrap();
    assert!(preview_body.contains("src/main.rs"));

    // Policy glob sanity check.
    let raw = std::fs::read_to_string(crate_root().join("workspace-policy.toml")).unwrap();
    let policy: WorkspacePolicy = toml::from_str(&raw).unwrap();
    let primary = policy.artifacts.entries.get("primary").unwrap();
    let preview = policy.artifacts.entries.get("preview").unwrap();
    assert!(
        path_matches_glob(&out.diff_path, primary),
        "diff path must match patches/*.diff"
    );
    assert!(
        path_matches_glob(&out.preview_path, preview),
        "preview path must match patches/*.files.txt"
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
