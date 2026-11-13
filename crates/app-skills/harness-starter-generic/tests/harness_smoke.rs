//! Smoke test for `harness-starter-generic`.
//!
//! Asserts:
//! (a) the workspace policy parses via
//!     `octos_agent::workspace_policy::WorkspacePolicy`.
//! (b) at least one declared artifact is produced under a fake run.
//! (c) `lifecycle_state` transitions `Queued -> Running -> Verifying -> Ready`
//!     via `octos_agent::task_supervisor::TaskSupervisor`.

use std::path::{Path, PathBuf};

use harness_starter_generic::{ProduceArtifactInput, produce_artifact};
use octos_agent::task_supervisor::{TaskLifecycleState, TaskRuntimeState, TaskSupervisor};
use octos_agent::workspace_policy::WorkspacePolicy;

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn policy_path() -> PathBuf {
    crate_root().join("workspace-policy.toml")
}

#[test]
fn tool_produces_primary_artifact_matching_policy_glob() {
    let tmp = tempfile::tempdir().unwrap();
    let input = ProduceArtifactInput {
        label: "generic-smoke".into(),
    };
    let out = produce_artifact(tmp.path(), &input).expect("produce artifact");

    let full = tmp.path().join(&out.artifact_path);
    assert!(full.exists(), "artifact file must exist");
    // Matches the `primary = "output/artifact-*.txt"` glob.
    assert!(
        out.artifact_path
            .to_string_lossy()
            .replace('\\', "/")
            .starts_with("output/artifact-")
    );
    assert!(out.artifact_path.extension().and_then(|e| e.to_str()) == Some("txt"));
    let body = std::fs::read_to_string(&full).unwrap();
    assert!(body.contains("generic-smoke"));

    // Sanity: the artifact glob in the shipped policy resolves to this path.
    let policy_raw = std::fs::read_to_string(policy_path()).unwrap();
    let policy: WorkspacePolicy = toml::from_str(&policy_raw).unwrap();
    let pattern = policy.artifacts.entries.get("primary").unwrap();
    assert!(
        path_matches_glob(&out.artifact_path, pattern),
        "produced path {} must match glob {}",
        out.artifact_path.display(),
        pattern
    );
}

#[test]
fn lifecycle_state_transitions_queued_running_verifying_ready() {
    let supervisor = TaskSupervisor::new();
    let id = supervisor.register("produce_artifact", "call-smoke", None);

    // Queued: just registered.
    let task = supervisor.get_task(&id).expect("task registered");
    assert_eq!(task.lifecycle_state(), TaskLifecycleState::Queued);

    // Running: tool began executing.
    supervisor.mark_running(&id);
    let task = supervisor.get_task(&id).unwrap();
    assert_eq!(task.lifecycle_state(), TaskLifecycleState::Running);

    // Verifying: runtime entered verification phase.
    supervisor.mark_runtime_state(
        &id,
        TaskRuntimeState::VerifyingOutputs,
        Some("verify produce_artifact primary".to_string()),
    );
    let task = supervisor.get_task(&id).unwrap();
    assert_eq!(task.lifecycle_state(), TaskLifecycleState::Verifying);

    // Ready: delivery succeeded.
    supervisor.mark_completed(&id, vec!["output/artifact-final.txt".to_string()]);
    let task = supervisor.get_task(&id).unwrap();
    assert_eq!(task.lifecycle_state(), TaskLifecycleState::Ready);
    assert_eq!(task.output_files, vec!["output/artifact-final.txt"]);
}

/// Minimal glob check that mirrors the set of patterns used by starter
/// policies (suffix + single `*` body segment). It is NOT a general glob
/// engine — the runtime uses the `glob` crate with gitignore semantics.
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
