//! Smoke test for `harness-starter-audio`.

use std::path::{Path, PathBuf};

use harness_starter_audio::{SynthesizeClipInput, synthesize_clip};
use octos_agent::workspace_policy::WorkspacePolicy;

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn tool_produces_wav_satisfying_size_validator() {
    let tmp = tempfile::tempdir().unwrap();
    let input = SynthesizeClipInput {
        label: "audio-smoke".into(),
        duration_ms: Some(500),
    };
    let out = synthesize_clip(tmp.path(), &input).unwrap();

    let full = tmp.path().join(&out.artifact_path);
    assert!(full.exists(), "wav file must exist");
    assert!(out.byte_len >= 4096, "must satisfy file_size_min:4096");

    // Sanity: the shipped policy's glob matches.
    let raw = std::fs::read_to_string(crate_root().join("workspace-policy.toml")).unwrap();
    let policy: WorkspacePolicy = toml::from_str(&raw).unwrap();
    let pattern = policy
        .artifacts
        .entries
        .get("primary_audio")
        .expect("primary_audio artifact");
    assert!(
        path_matches_glob(&out.artifact_path, pattern),
        "produced path must match audio/*.wav"
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
