//! Smoke test for `harness-starter-report`.
//!
//! Matches the 4-part acceptance spec in
//! `docs/OCTOS_HARNESS_DEVELOPER_GUIDE.md` Part 2 Step 5.

use std::path::{Path, PathBuf};

use harness_starter_report::{GenerateReportInput, generate_report};
use octos_agent::task_supervisor::{TaskLifecycleState, TaskRuntimeState, TaskSupervisor};
use octos_agent::workspace_policy::{WorkspacePolicy, WorkspacePolicyKind};
use octos_plugin::PluginManifest;

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn manifest_parses_and_declares_generate_report_tool() {
    let manifest = PluginManifest::from_file(&crate_root().join("manifest.json"))
        .expect("manifest.json must parse");
    assert_eq!(manifest.id, "harness-starter-report");
    let tool = manifest
        .tools
        .iter()
        .find(|t| t.name == "generate_report")
        .expect("generate_report tool missing");
    let topic_field = tool
        .input_schema
        .get("properties")
        .and_then(|v| v.get("topic"));
    assert!(
        topic_field.is_some(),
        "input_schema.properties.topic must exist"
    );
}

#[test]
fn workspace_policy_parses_and_binds_generate_report_to_primary() {
    let raw = std::fs::read_to_string(crate_root().join("workspace-policy.toml"))
        .expect("policy readable");
    let policy: WorkspacePolicy = toml::from_str(&raw).expect("policy parses");

    assert_eq!(policy.workspace.kind, WorkspacePolicyKind::Session);
    assert_eq!(
        policy.artifacts.entries.get("primary").map(String::as_str),
        Some("reports/*.md")
    );

    let task = policy
        .spawn_tasks
        .get("generate_report")
        .expect("spawn_tasks.generate_report");
    assert_eq!(task.artifact_sources(), vec!["primary"]);
    assert!(
        task.on_verify
            .iter()
            .any(|action| action == "file_size_min:$primary:256"),
        "must declare file_size_min validator"
    );
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

#[test]
fn lifecycle_state_transitions_queued_running_verifying_ready() {
    let supervisor = TaskSupervisor::new();
    let id = supervisor.register("generate_report", "call-report", None);

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
        Some("verify generate_report primary".to_string()),
    );
    assert_eq!(
        supervisor.get_task(&id).unwrap().lifecycle_state(),
        TaskLifecycleState::Verifying,
    );
    supervisor.mark_completed(&id, vec!["reports/stub.md".to_string()]);
    assert_eq!(
        supervisor.get_task(&id).unwrap().lifecycle_state(),
        TaskLifecycleState::Ready,
    );
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
