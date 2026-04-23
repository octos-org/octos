//! Integration tests for the declarative validator runner (harness M4.3).
//!
//! These tests exercise typed validator specs, evidence capture, timeout
//! behaviour, replay through the persisted ledger, and operator counters.
//!
//! Run with `cargo test -p octos-agent --test validator_runner`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use octos_agent::validators::{
    VALIDATOR_RESULT_SCHEMA_VERSION, ValidatorInvocation, ValidatorLedger, ValidatorPhase,
    ValidatorRunner, ValidatorStatus,
};
use octos_agent::workspace_policy::{Validator, ValidatorPhaseKind, ValidatorSpec};
use octos_agent::{Tool, ToolRegistry, ToolResult};
use serde_json::{Value, json};
use tempfile::tempdir;

fn command_validator(id: &str, cmd: &str, args: &[&str]) -> Validator {
    Validator {
        id: id.to_string(),
        required: true,
        timeout_ms: Some(5000),
        phase: ValidatorPhaseKind::Completion,
        spec: ValidatorSpec::Command {
            cmd: cmd.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
        },
    }
}

fn file_exists_validator(id: &str, path: &str, min_bytes: Option<u64>) -> Validator {
    Validator {
        id: id.to_string(),
        required: true,
        timeout_ms: None,
        phase: ValidatorPhaseKind::Completion,
        spec: ValidatorSpec::FileExists {
            path: path.to_string(),
            min_bytes,
        },
    }
}

fn tool_call_validator(id: &str, tool: &str, args: Value) -> Validator {
    Validator {
        id: id.to_string(),
        required: true,
        timeout_ms: Some(5000),
        phase: ValidatorPhaseKind::Completion,
        spec: ValidatorSpec::ToolCall {
            tool: tool.to_string(),
            args,
        },
    }
}

/// A minimal tool that always succeeds (for ToolCall validator tests).
struct OkTool;

#[async_trait]
impl Tool for OkTool {
    fn name(&self) -> &str {
        "ok"
    }

    fn description(&self) -> &str {
        "test tool"
    }

    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }

    async fn execute(&self, _args: &Value) -> eyre::Result<ToolResult> {
        Ok(ToolResult {
            success: true,
            output: "ok".into(),
            ..Default::default()
        })
    }
}

/// A minimal tool that always fails.
struct FailTool;

#[async_trait]
impl Tool for FailTool {
    fn name(&self) -> &str {
        "fail"
    }

    fn description(&self) -> &str {
        "test tool"
    }

    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }

    async fn execute(&self, _args: &Value) -> eyre::Result<ToolResult> {
        Ok(ToolResult {
            success: false,
            output: "tool reports failure".into(),
            ..Default::default()
        })
    }
}

fn registry_with_tools() -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    registry.register(OkTool);
    registry.register(FailTool);
    Arc::new(registry)
}

#[tokio::test]
async fn should_block_ready_when_required_command_validator_fails() {
    let dir = tempdir().unwrap();
    let runner = ValidatorRunner::new(registry_with_tools(), dir.path().to_path_buf());

    // Fails intentionally: exit 1
    let validators = vec![Validator {
        id: "cmd_fail".into(),
        required: true,
        timeout_ms: Some(3000),
        phase: ValidatorPhaseKind::Completion,
        spec: ValidatorSpec::Command {
            cmd: "sh".into(),
            args: vec!["-c".into(), "exit 1".into()],
        },
    }];

    let invocation = ValidatorInvocation {
        phase: ValidatorPhase::Completion,
        workspace_root: dir.path().to_path_buf(),
        repo_label: "slides/demo".into(),
    };
    let outcomes = runner.run_all(&invocation, &validators).await;

    assert_eq!(outcomes.len(), 1);
    let outcome = &outcomes[0];
    assert_eq!(outcome.validator_id, "cmd_fail");
    assert_eq!(outcome.status, ValidatorStatus::Fail);
    assert_eq!(outcome.schema_version, VALIDATOR_RESULT_SCHEMA_VERSION);
    assert_eq!(outcome.schema_version, 1);
    assert!(outcome.required);
    assert!(!outcomes.iter().all(|o| o.required_gate_passed()));
}

#[tokio::test]
async fn should_warn_but_not_block_when_optional_validator_fails() {
    let dir = tempdir().unwrap();
    let runner = ValidatorRunner::new(registry_with_tools(), dir.path().to_path_buf());

    let validators = vec![Validator {
        id: "optional_fail".into(),
        required: false,
        timeout_ms: Some(3000),
        phase: ValidatorPhaseKind::Completion,
        spec: ValidatorSpec::FileExists {
            path: "does-not-exist.txt".into(),
            min_bytes: None,
        },
    }];

    let invocation = ValidatorInvocation {
        phase: ValidatorPhase::Completion,
        workspace_root: dir.path().to_path_buf(),
        repo_label: "slides/demo".into(),
    };
    let outcomes = runner.run_all(&invocation, &validators).await;

    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].status, ValidatorStatus::Fail);
    assert!(!outcomes[0].required);
    // Required gate passes because the only failure is optional.
    assert!(outcomes.iter().all(|o| o.required_gate_passed()));
}

#[tokio::test]
async fn should_record_duration_and_evidence_path_for_command_validator() {
    let dir = tempdir().unwrap();
    let runner = ValidatorRunner::new(registry_with_tools(), dir.path().to_path_buf());

    let validators = vec![Validator {
        id: "echo_cmd".into(),
        required: true,
        timeout_ms: Some(3000),
        phase: ValidatorPhaseKind::Completion,
        spec: ValidatorSpec::Command {
            cmd: "sh".into(),
            args: vec!["-c".into(), "echo hello".into()],
        },
    }];

    let invocation = ValidatorInvocation {
        phase: ValidatorPhase::TurnEnd,
        workspace_root: dir.path().to_path_buf(),
        repo_label: "slides/demo".into(),
    };
    let outcomes = runner.run_all(&invocation, &validators).await;

    let outcome = &outcomes[0];
    assert_eq!(outcome.status, ValidatorStatus::Pass);
    // duration_ms is recorded (non-zero for a real subprocess invocation is typical,
    // but we only assert it's set — 0ms is still a valid recorded value).
    assert!(outcome.duration_ms < 10_000);
    let evidence_path = outcome
        .evidence_path
        .as_ref()
        .expect("command validator must record evidence path");
    assert!(evidence_path.exists(), "evidence file must be written");
    let evidence = std::fs::read_to_string(evidence_path).unwrap();
    assert!(evidence.contains("hello"), "evidence captures stdout");
}

#[tokio::test]
async fn should_expose_stderr_in_outcome_for_operator_visibility() {
    let dir = tempdir().unwrap();
    let runner = ValidatorRunner::new(registry_with_tools(), dir.path().to_path_buf());

    let validators = vec![Validator {
        id: "stderr_cmd".into(),
        required: true,
        timeout_ms: Some(3000),
        phase: ValidatorPhaseKind::Completion,
        spec: ValidatorSpec::Command {
            cmd: "sh".into(),
            args: vec!["-c".into(), "echo hello >&2; exit 1".into()],
        },
    }];

    let invocation = ValidatorInvocation {
        phase: ValidatorPhase::Completion,
        workspace_root: dir.path().to_path_buf(),
        repo_label: "slides/demo".into(),
    };
    let outcomes = runner.run_all(&invocation, &validators).await;

    let outcome = &outcomes[0];
    assert_eq!(outcome.status, ValidatorStatus::Fail);
    let stderr = outcome
        .stderr
        .as_ref()
        .expect("stderr should be captured when command fails with stderr output");
    assert!(stderr.contains("hello"));
}

#[tokio::test]
async fn should_kill_child_process_on_validator_timeout() {
    let dir = tempdir().unwrap();
    let runner = ValidatorRunner::new(registry_with_tools(), dir.path().to_path_buf());

    // Use a sentinel file the child touches — once removed by the kill handler,
    // we know the child was actually reaped. The child sleeps far longer than the
    // validator timeout so it must be killed, not naturally exit.
    let sentinel = dir.path().join("sentinel");
    let sentinel_path = sentinel.display().to_string();
    let pid_file = dir.path().join("child.pid");
    let pid_path = pid_file.display().to_string();

    let validators = vec![Validator {
        id: "timeout_cmd".into(),
        required: true,
        timeout_ms: Some(300),
        phase: ValidatorPhaseKind::Completion,
        spec: ValidatorSpec::Command {
            cmd: "sh".into(),
            args: vec![
                "-c".into(),
                format!(
                    "echo $$ > {pid_path}; touch {sentinel_path}; sleep 30",
                    pid_path = pid_path,
                    sentinel_path = sentinel_path,
                ),
            ],
        },
    }];

    let invocation = ValidatorInvocation {
        phase: ValidatorPhase::Completion,
        workspace_root: dir.path().to_path_buf(),
        repo_label: "slides/demo".into(),
    };

    let before = std::time::Instant::now();
    let outcomes = runner.run_all(&invocation, &validators).await;
    let elapsed = before.elapsed();

    assert!(
        elapsed < Duration::from_secs(10),
        "runner must kill child and return before sleep finishes; elapsed = {:?}",
        elapsed
    );

    let outcome = &outcomes[0];
    assert_eq!(outcome.status, ValidatorStatus::Timeout);
    assert!(
        outcome.reason.to_lowercase().contains("timeout") || outcome.reason.contains("timed out")
    );

    // PID probe: give the OS a moment, then verify the child is truly gone.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let pid_raw = std::fs::read_to_string(&pid_file).unwrap_or_default();
    let pid: i32 = pid_raw.trim().parse().unwrap_or(0);
    if pid > 0 {
        // `kill -0 <pid>` succeeds if and only if the PID is alive.
        let status = std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .status();
        if let Ok(status) = status {
            assert!(
                !status.success(),
                "child PID {pid} must be dead after validator timeout (sandbox-exec or sh wrapper)"
            );
        }
    }
}

#[tokio::test]
async fn should_pass_file_exists_validator_when_file_meets_size_floor() {
    let dir = tempdir().unwrap();
    let runner = ValidatorRunner::new(registry_with_tools(), dir.path().to_path_buf());

    std::fs::write(dir.path().join("artifact.bin"), vec![0u8; 2048]).unwrap();

    let validators = vec![file_exists_validator("file_ok", "artifact.bin", Some(1024))];
    let invocation = ValidatorInvocation {
        phase: ValidatorPhase::Completion,
        workspace_root: dir.path().to_path_buf(),
        repo_label: "slides/demo".into(),
    };
    let outcomes = runner.run_all(&invocation, &validators).await;

    assert_eq!(outcomes[0].status, ValidatorStatus::Pass);
}

#[tokio::test]
async fn should_fail_file_exists_validator_when_size_under_floor() {
    let dir = tempdir().unwrap();
    let runner = ValidatorRunner::new(registry_with_tools(), dir.path().to_path_buf());

    std::fs::write(dir.path().join("artifact.bin"), b"x").unwrap();

    let validators = vec![file_exists_validator(
        "file_small",
        "artifact.bin",
        Some(1024),
    )];
    let invocation = ValidatorInvocation {
        phase: ValidatorPhase::Completion,
        workspace_root: dir.path().to_path_buf(),
        repo_label: "slides/demo".into(),
    };
    let outcomes = runner.run_all(&invocation, &validators).await;

    assert_eq!(outcomes[0].status, ValidatorStatus::Fail);
    assert!(outcomes[0].reason.contains("min_bytes") || outcomes[0].reason.contains("bytes"));
}

#[tokio::test]
async fn should_pass_tool_call_validator_when_tool_succeeds() {
    let dir = tempdir().unwrap();
    let runner = ValidatorRunner::new(registry_with_tools(), dir.path().to_path_buf());

    let validators = vec![tool_call_validator("tool_ok", "ok", json!({}))];
    let invocation = ValidatorInvocation {
        phase: ValidatorPhase::Completion,
        workspace_root: dir.path().to_path_buf(),
        repo_label: "slides/demo".into(),
    };
    let outcomes = runner.run_all(&invocation, &validators).await;

    assert_eq!(outcomes[0].status, ValidatorStatus::Pass);
}

#[tokio::test]
async fn should_fail_tool_call_validator_when_tool_reports_unsuccessful() {
    let dir = tempdir().unwrap();
    let runner = ValidatorRunner::new(registry_with_tools(), dir.path().to_path_buf());

    let validators = vec![tool_call_validator("tool_fail", "fail", json!({}))];
    let invocation = ValidatorInvocation {
        phase: ValidatorPhase::Completion,
        workspace_root: dir.path().to_path_buf(),
        repo_label: "slides/demo".into(),
    };
    let outcomes = runner.run_all(&invocation, &validators).await;

    assert_eq!(outcomes[0].status, ValidatorStatus::Fail);
    assert!(outcomes[0].reason.contains("tool reports failure"));
}

#[tokio::test]
async fn should_persist_outcomes_and_replay_them_byte_for_byte() {
    let dir = tempdir().unwrap();
    let ledger_path = dir.path().join("validator_ledger.jsonl");
    let ledger = ValidatorLedger::open(ledger_path.clone()).unwrap();

    let runner = ValidatorRunner::new(registry_with_tools(), dir.path().to_path_buf())
        .with_ledger(ledger.clone());

    std::fs::write(dir.path().join("artifact.bin"), vec![0u8; 4096]).unwrap();

    let validators = vec![
        file_exists_validator("file_ok", "artifact.bin", Some(1024)),
        command_validator("cmd_ok", "sh", &["-c", "echo hi"]),
    ];
    let invocation = ValidatorInvocation {
        phase: ValidatorPhase::Completion,
        workspace_root: dir.path().to_path_buf(),
        repo_label: "slides/demo".into(),
    };
    let original = runner.run_all(&invocation, &validators).await;
    assert_eq!(original.len(), 2);

    // Replay: re-open the ledger and read the persisted outcomes from disk.
    let reopened = ValidatorLedger::open(ledger_path).unwrap();
    let replayed = reopened.read_all().unwrap();

    assert_eq!(replayed.len(), original.len());
    for (live, restored) in original.iter().zip(replayed.iter()) {
        assert_eq!(
            serde_json::to_value(live).unwrap(),
            serde_json::to_value(restored).unwrap(),
            "replayed outcome must match byte-for-byte"
        );
        assert_eq!(restored.schema_version, 1);
    }
}

#[tokio::test]
async fn should_strip_blocked_env_vars_from_command_validator_child() {
    // Spawn a re-entrant child that pre-seeds LD_PRELOAD in its own env, then
    // asks the runner to spawn a command validator. The runner must strip
    // BLOCKED_ENV_VARS before invoking sh.
    let dir = tempdir().unwrap();
    let runner = ValidatorRunner::new(registry_with_tools(), dir.path().to_path_buf());

    // Probe the blocked-env sanitization path directly. The runner pre-clears
    // every BLOCKED_ENV_VAR on the child Command, even if set earlier on the
    // Command builder, so the child process sees an empty variable.
    let probe_path = dir.path().join("probe.txt");
    let validators = vec![Validator {
        id: "env_probe".into(),
        required: true,
        timeout_ms: Some(5000),
        phase: ValidatorPhaseKind::Completion,
        spec: ValidatorSpec::Command {
            cmd: "sh".into(),
            args: vec![
                "-c".into(),
                format!(
                    "printf '%s' \"${{LD_PRELOAD:-__unset__}}\" > {}",
                    probe_path.display()
                ),
            ],
        },
    }];
    let invocation = ValidatorInvocation {
        phase: ValidatorPhase::Completion,
        workspace_root: dir.path().to_path_buf(),
        repo_label: "slides/demo".into(),
    };
    // Invoke via a helper that pre-seeds LD_PRELOAD on the spawned command.
    runner
        .run_all_with_seeded_env(&invocation, &validators, &[("LD_PRELOAD", "/tmp/leak.so")])
        .await;

    let probe = std::fs::read_to_string(&probe_path).unwrap_or_default();
    assert_eq!(
        probe.trim(),
        "__unset__",
        "LD_PRELOAD must be stripped from command validator child, got {probe:?}"
    );
}

#[tokio::test]
async fn should_block_spawn_task_contract_when_required_validator_fails() {
    use octos_agent::workspace_contract::{SpawnTaskContractResult, enforce_spawn_task_contract};
    use octos_agent::workspace_policy::{
        Validator, ValidatorPhaseKind, ValidatorSpec, WorkspacePolicy, WorkspaceSpawnTaskPolicy,
        write_workspace_policy,
    };
    use std::time::UNIX_EPOCH;

    let dir = tempdir().unwrap();
    let mut policy = WorkspacePolicy::for_session();
    // Add a required validator that cannot pass: the artifact is a single byte
    // but the validator requires at least 1KiB.
    policy.artifacts.entries.clear();
    policy
        .artifacts
        .entries
        .insert("primary_audio".into(), "*.mp3".into());
    policy.validation.validators = vec![Validator {
        id: "min_bytes_gate".into(),
        required: true,
        timeout_ms: None,
        phase: ValidatorPhaseKind::Completion,
        spec: ValidatorSpec::FileExists {
            path: "tts_result.mp3".into(),
            min_bytes: Some(1024),
        },
    }];
    policy.spawn_tasks.insert(
        "fm_tts".into(),
        WorkspaceSpawnTaskPolicy {
            artifact: Some("primary_audio".into()),
            artifacts: Vec::new(),
            on_verify: vec!["file_exists:$artifact".into()],
            on_complete: Vec::new(),
            on_deliver: Vec::new(),
            on_failure: vec!["notify_user:validator gate failed".into()],
        },
    );
    write_workspace_policy(dir.path(), &policy).unwrap();

    // Tiny artifact that passes file_exists but fails min_bytes.
    std::fs::write(dir.path().join("tts_result.mp3"), vec![0u8; 16]).unwrap();

    let registry = ToolRegistry::with_builtins(dir.path());
    let result = enforce_spawn_task_contract(
        &registry,
        "fm_tts",
        "tool-call-validator-1",
        &[],
        UNIX_EPOCH,
        None,
    )
    .await;

    match result {
        SpawnTaskContractResult::Failed { error, .. } => {
            assert!(
                error.contains("min_bytes_gate")
                    || error.contains("min_bytes")
                    || error.contains("validator"),
                "expected validator-gate failure, got {error}"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }

    // And the ledger must have a persisted outcome for replay.
    let ledger_path = dir.path().join(".octos").join("validator_outcomes.jsonl");
    assert!(ledger_path.exists(), "ledger must be created");
    let ledger = ValidatorLedger::open(ledger_path).unwrap();
    let outcomes = ledger.read_all().unwrap();
    assert!(
        outcomes
            .iter()
            .any(|o| o.validator_id == "min_bytes_gate" && o.status == ValidatorStatus::Fail)
    );
}

#[tokio::test]
async fn should_not_block_spawn_task_contract_when_optional_validator_fails() {
    use octos_agent::workspace_contract::{SpawnTaskContractResult, enforce_spawn_task_contract};
    use octos_agent::workspace_policy::{
        Validator, ValidatorPhaseKind, ValidatorSpec, WorkspacePolicy, WorkspaceSpawnTaskPolicy,
        write_workspace_policy,
    };
    use std::time::UNIX_EPOCH;

    let dir = tempdir().unwrap();
    let mut policy = WorkspacePolicy::for_session();
    policy.artifacts.entries.clear();
    policy
        .artifacts
        .entries
        .insert("primary_audio".into(), "*.mp3".into());
    policy.validation.validators = vec![Validator {
        id: "optional_warn".into(),
        required: false,
        timeout_ms: None,
        phase: ValidatorPhaseKind::Completion,
        spec: ValidatorSpec::FileExists {
            path: "never-here.txt".into(),
            min_bytes: None,
        },
    }];
    policy.spawn_tasks.insert(
        "fm_tts".into(),
        WorkspaceSpawnTaskPolicy {
            artifact: Some("primary_audio".into()),
            artifacts: Vec::new(),
            on_verify: vec!["file_exists:$artifact".into()],
            on_complete: Vec::new(),
            on_deliver: Vec::new(),
            on_failure: Vec::new(),
        },
    );
    write_workspace_policy(dir.path(), &policy).unwrap();

    std::fs::write(dir.path().join("tts_result.mp3"), vec![0u8; 2048]).unwrap();

    let registry = ToolRegistry::with_builtins(dir.path());
    let result = enforce_spawn_task_contract(
        &registry,
        "fm_tts",
        "tool-call-validator-2",
        &[],
        UNIX_EPOCH,
        None,
    )
    .await;

    assert!(
        matches!(result, SpawnTaskContractResult::Satisfied { .. }),
        "optional validator must not block, got {result:?}"
    );

    let ledger_path = dir.path().join(".octos").join("validator_outcomes.jsonl");
    let ledger = ValidatorLedger::open(ledger_path).unwrap();
    let outcomes = ledger.read_all().unwrap();
    let warn = outcomes
        .iter()
        .find(|o| o.validator_id == "optional_warn")
        .expect("optional warning must be persisted");
    assert!(!warn.required);
    assert_eq!(warn.status, ValidatorStatus::Fail);
}

#[tokio::test]
async fn should_reflect_required_validator_fail_in_inspect_ready_flag() {
    use octos_agent::workspace_git::WorkspaceProjectKind;
    use octos_agent::workspace_git::inspect_workspace_contract_at_root;
    use octos_agent::workspace_policy::{
        Validator, ValidatorPhaseKind, ValidatorSpec, WorkspacePolicy, write_workspace_policy,
    };

    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("slides").join("demo");
    std::fs::create_dir_all(&repo).unwrap();
    let mut policy = WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides);
    policy.validation.validators = vec![Validator {
        id: "gate".into(),
        required: true,
        timeout_ms: None,
        phase: ValidatorPhaseKind::Completion,
        spec: ValidatorSpec::FileExists {
            path: "output/deck.pptx".into(),
            min_bytes: None,
        },
    }];
    write_workspace_policy(&repo, &policy).unwrap();

    // Make all existing artifacts and turn-end checks pass.
    std::fs::write(repo.join("script.js"), "// slides").unwrap();
    std::fs::write(repo.join("memory.md"), "# memory").unwrap();
    std::fs::write(repo.join("changelog.md"), "# changelog").unwrap();
    std::fs::create_dir_all(repo.join("output")).unwrap();
    std::fs::write(repo.join("output/deck.pptx"), b"deck").unwrap();
    std::fs::write(repo.join("output/imgs/slide-01.png"), b"png").ok();
    std::fs::create_dir_all(repo.join("output/imgs")).unwrap();
    std::fs::write(repo.join("output/imgs/slide-01.png"), b"png").unwrap();

    // Before any validator runs, inspect must flag ready=false because the
    // declared required validator has no Pass outcome yet.
    let status = inspect_workspace_contract_at_root(&repo).unwrap();
    assert!(
        !status.ready,
        "ready must be false until the required validator has a persisted Pass outcome"
    );

    // Persist a Pass outcome for the gate and re-inspect.
    let ledger_path = repo.join(".octos").join("validator_outcomes.jsonl");
    let ledger = ValidatorLedger::open(&ledger_path).unwrap();
    let outcome = octos_agent::validators::ValidatorOutcome {
        schema_version: VALIDATOR_RESULT_SCHEMA_VERSION,
        validator_id: "gate".into(),
        phase: ValidatorPhase::Completion,
        kind: "file_exists".into(),
        repo_label: "slides/demo".into(),
        required: true,
        status: ValidatorStatus::Pass,
        reason: "manual seed".into(),
        duration_ms: 0,
        evidence_path: None,
        stderr: None,
        started_at: chrono::Utc::now(),
    };
    ledger.append(&outcome).unwrap();

    let status = inspect_workspace_contract_at_root(&repo).unwrap();
    assert!(
        status.ready,
        "ready must be true after required validator passes"
    );
    assert_eq!(status.validator_outcomes.len(), 1);
    assert_eq!(status.validator_outcomes[0].validator_id, "gate");
    assert_eq!(status.optional_validator_warnings, 0);
}

#[tokio::test]
async fn should_block_command_validator_with_dangerous_pattern() {
    // `rm -rf /` is denied by SafePolicy. The runner must refuse to spawn it
    // and surface a typed Fail result instead.
    let dir = tempdir().unwrap();
    let runner = ValidatorRunner::new(registry_with_tools(), dir.path().to_path_buf());

    let validators = vec![Validator {
        id: "dangerous".into(),
        required: true,
        timeout_ms: Some(3000),
        phase: ValidatorPhaseKind::Completion,
        spec: ValidatorSpec::Command {
            cmd: "sh".into(),
            args: vec!["-c".into(), "rm -rf /".into()],
        },
    }];
    let invocation = ValidatorInvocation {
        phase: ValidatorPhase::Completion,
        workspace_root: dir.path().to_path_buf(),
        repo_label: "slides/demo".into(),
    };
    let outcomes = runner.run_all(&invocation, &validators).await;

    let outcome = &outcomes[0];
    assert_eq!(outcome.status, ValidatorStatus::Error);
    assert!(
        outcome.reason.to_lowercase().contains("denied")
            || outcome.reason.to_lowercase().contains("policy")
    );
}
