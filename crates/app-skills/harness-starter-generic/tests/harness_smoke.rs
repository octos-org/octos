//! Smoke test for `harness-starter-generic`.
//!
//! Asserts:
//! (a) the plugin manifest parses via `octos_plugin::PluginManifest::from_file`.
//! (b) the workspace policy parses via
//!     `octos_agent::workspace_policy::WorkspacePolicy`.
//! (c) at least one declared artifact is produced under a fake run.
//! (d) `lifecycle_state` transitions `Queued -> Running -> Verifying -> Ready`
//!     via `octos_agent::task_supervisor::TaskSupervisor`.

use std::path::{Path, PathBuf};

use harness_starter_generic::{ProduceArtifactInput, produce_artifact};
use octos_agent::task_supervisor::{TaskLifecycleState, TaskRuntimeState, TaskSupervisor};
use octos_agent::workspace_policy::{WorkspacePolicy, WorkspacePolicyKind};
use octos_plugin::PluginManifest;

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn manifest_path() -> PathBuf {
    crate_root().join("manifest.json")
}

fn policy_path() -> PathBuf {
    crate_root().join("workspace-policy.toml")
}

#[test]
fn manifest_parses_and_declares_spawn_only_tool() {
    let manifest = PluginManifest::from_file(&manifest_path()).expect("manifest.json must parse");
    assert_eq!(manifest.id, "harness-starter-generic");
    assert!(!manifest.tools.is_empty(), "manifest must declare a tool");
    let tool = &manifest.tools[0];
    assert_eq!(tool.name, "produce_artifact");
}

#[test]
fn workspace_policy_parses_and_declares_primary_artifact() {
    let raw =
        std::fs::read_to_string(policy_path()).expect("workspace-policy.toml must be readable");
    let policy: WorkspacePolicy = toml::from_str(&raw).expect("workspace-policy.toml must parse");

    assert_eq!(policy.workspace.kind, WorkspacePolicyKind::Session);
    assert_eq!(
        policy.artifacts.entries.get("primary").map(String::as_str),
        Some("output/artifact-*.txt"),
        "starter policy must declare primary = output/artifact-*.txt"
    );

    let task = policy
        .spawn_tasks
        .get("produce_artifact")
        .expect("spawn_tasks.produce_artifact must exist");
    assert_eq!(
        task.artifact_sources(),
        vec!["primary"],
        "spawn task must point at the primary artifact"
    );
    assert!(
        task.on_verify
            .iter()
            .any(|action| action == "file_exists:$primary"),
        "spawn task must include file_exists:$primary in on_verify"
    );
    assert!(
        !task.on_failure.is_empty(),
        "spawn task must declare an on_failure action"
    );
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
    let path_str = path.to_string_lossy();
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
