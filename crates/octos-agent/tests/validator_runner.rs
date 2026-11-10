//! Windows AppContainer fail-closed regression for the validator runner.
#![cfg(windows)]

use std::sync::Arc;

use octos_agent::ToolRegistry;
use octos_agent::validators::{
    ValidatorInvocation, ValidatorPhase, ValidatorRunner, ValidatorStatus,
};
use octos_agent::workspace_policy::{Validator, ValidatorPhaseKind, ValidatorSpec};
use tempfile::tempdir;

fn command_validator(id: &str, cmd: &str, args: &[&str]) -> Validator {
    Validator {
        id: id.to_string(),
        required: true,
        soft_fail: false,
        timeout_ms: Some(5000),
        phase: ValidatorPhaseKind::Completion,
        spec: ValidatorSpec::Command {
            cmd: cmd.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
        },
    }
}

/// A missing `octos-sandbox.exe` must never turn into a raw host spawn.
/// The marker file proves the validator argv was not executed directly.
#[tokio::test]
async fn windows_command_validator_fails_closed_when_appcontainer_helper_vanished() {
    let dir = tempdir().unwrap();
    let marker = dir.path().join("bare-host-spawn-marker");
    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf())
        .with_sandbox(Arc::new(octos_agent::sandbox::AppContainerSandbox {
            allow_network: false,
            read_allow_paths: vec![],
            profile_name: None,
            workspace_write: true,
        }));

    let marker_str = marker.to_string_lossy().to_string();
    let validators = vec![command_validator(
        "win_fail_closed",
        "cmd",
        &["/C", "copy", "NUL", &marker_str],
    )];
    let invocation = ValidatorInvocation {
        phase: ValidatorPhase::Completion,
        workspace_root: dir.path().to_path_buf(),
        repo_label: "sandbox/appcontainer".into(),
        input_args: None,
        tool_output: None,
        spawn_only_files: Vec::new(),
    };
    let outcomes = runner.run_all(&invocation, &validators).await;

    assert_eq!(outcomes.len(), 1);
    let outcome = &outcomes[0];
    assert_eq!(
        outcome.status,
        ValidatorStatus::Error,
        "helper-vanished AppContainer must fail closed, got {outcome:?}"
    );
    assert!(
        outcome
            .reason
            .contains("not supported under the active sandbox backend"),
        "the cannot-wrap refusal explains itself: {}",
        outcome.reason
    );
    assert!(
        !marker.exists(),
        "the validator argv must NEVER be spawned raw on the host"
    );
}
