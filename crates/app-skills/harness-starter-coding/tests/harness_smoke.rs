//! Smoke test for `harness-starter-coding`.

use std::path::{Path, PathBuf};

use harness_starter_coding::{PatchHunk, ProposePatchInput, propose_patch};
use octos_agent::task_supervisor::{TaskLifecycleState, TaskRuntimeState, TaskSupervisor};
use octos_agent::workspace_policy::{WorkspacePolicy, WorkspacePolicyKind};
use octos_plugin::PluginManifest;

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn manifest_parses_and_declares_propose_patch_tool() {
    let manifest = PluginManifest::from_file(&crate_root().join("manifest.json"))
        .expect("manifest.json must parse");
    assert_eq!(manifest.id, "harness-starter-coding");
    assert!(manifest.tools.iter().any(|t| t.name == "propose_patch"));
}

#[test]
fn workspace_policy_binds_propose_patch_to_primary_and_preview() {
    let raw = std::fs::read_to_string(crate_root().join("workspace-policy.toml"))
        .expect("policy readable");
    let policy: WorkspacePolicy = toml::from_str(&raw).expect("policy parses");

    assert_eq!(policy.workspace.kind, WorkspacePolicyKind::Session);
    assert_eq!(
        policy.artifacts.entries.get("primary").map(String::as_str),
        Some("patches/*.diff"),
    );
    assert_eq!(
        policy.artifacts.entries.get("preview").map(String::as_str),
        Some("patches/*.files.txt"),
    );

    let task = policy
        .spawn_tasks
        .get("propose_patch")
        .expect("spawn_tasks.propose_patch");
    assert_eq!(task.artifact_sources(), vec!["primary", "preview"]);
    assert!(
        task.on_verify
            .iter()
            .any(|a| a == "file_size_min:$primary:64"),
        "must declare file_size_min for primary diff"
    );
    assert!(
        task.on_verify.iter().any(|a| a == "file_exists:$preview"),
        "must declare file_exists:$preview"
    );
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

#[test]
fn lifecycle_state_transitions_queued_running_verifying_ready() {
    let supervisor = TaskSupervisor::new();
    let id = supervisor.register("propose_patch", "call-coding", None);

    assert_eq!(
        supervisor.get_task(&id).unwrap().lifecycle_state(),
        TaskLifecycleState::Queued,
    );
    supervisor.mark_running(&id);
    assert_eq!(
        supervisor.get_task(&id).unwrap().lifecycle_state(),
        TaskLifecycleState::Running,
    );
    supervisor.mark_runtime_state(
        &id,
        TaskRuntimeState::VerifyingOutputs,
        Some("verify propose_patch artifacts".to_string()),
    );
    assert_eq!(
        supervisor.get_task(&id).unwrap().lifecycle_state(),
        TaskLifecycleState::Verifying,
    );
    supervisor.mark_completed(
        &id,
        vec![
            "patches/demo.diff".to_string(),
            "patches/demo.files.txt".to_string(),
        ],
    );
    let task = supervisor.get_task(&id).unwrap();
    assert_eq!(task.lifecycle_state(), TaskLifecycleState::Ready);
    assert_eq!(task.output_files.len(), 2);
}

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
