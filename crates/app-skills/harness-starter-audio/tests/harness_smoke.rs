//! Smoke test for `harness-starter-audio`.

use std::path::{Path, PathBuf};

use harness_starter_audio::{SynthesizeClipInput, synthesize_clip};
use octos_agent::task_supervisor::{TaskLifecycleState, TaskRuntimeState, TaskSupervisor};
use octos_agent::workspace_policy::{WorkspacePolicy, WorkspacePolicyKind};
use octos_plugin::PluginManifest;

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn manifest_parses_and_declares_synthesize_clip_tool() {
    let manifest = PluginManifest::from_file(&crate_root().join("manifest.json"))
        .expect("manifest.json must parse");
    assert_eq!(manifest.id, "harness-starter-audio");
    assert!(manifest.tools.iter().any(|t| t.name == "synthesize_clip"));
}

#[test]
fn workspace_policy_declares_primary_audio_with_size_validator() {
    let raw = std::fs::read_to_string(crate_root().join("workspace-policy.toml"))
        .expect("policy readable");
    let policy: WorkspacePolicy = toml::from_str(&raw).expect("policy parses");

    assert_eq!(policy.workspace.kind, WorkspacePolicyKind::Session);
    assert_eq!(
        policy
            .artifacts
            .entries
            .get("primary_audio")
            .map(String::as_str),
        Some("audio/*.wav"),
    );

    let task = policy
        .spawn_tasks
        .get("synthesize_clip")
        .expect("spawn_tasks.synthesize_clip");
    assert_eq!(task.artifact_sources(), vec!["primary_audio"]);
    assert!(
        task.on_verify
            .iter()
            .any(|action| action == "file_size_min:$primary_audio:4096"),
        "must declare file_size_min:4096 for primary_audio"
    );
    assert!(
        !task.on_failure.is_empty(),
        "must declare an on_failure action"
    );
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

#[test]
fn lifecycle_state_transitions_queued_running_verifying_ready() {
    let supervisor = TaskSupervisor::new();
    let id = supervisor.register("synthesize_clip", "call-audio", None);

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
        Some("verify synthesize_clip primary_audio".to_string()),
    );
    assert_eq!(
        supervisor.get_task(&id).unwrap().lifecycle_state(),
        TaskLifecycleState::Verifying,
    );
    supervisor.mark_completed(&id, vec!["audio/clip.wav".to_string()]);
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
