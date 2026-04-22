use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use glob::glob;

use crate::behaviour::{
    ActionContext, ActionResult, evaluate_actions_with_context, run_action_with_context,
};
use crate::task_supervisor::{TaskRuntimeState, TaskSupervisor};
use crate::tools::ToolRegistry;
use crate::validators::{
    ValidatorInvocation, ValidatorOutcome, ValidatorPhase, ValidatorRunner, ValidatorStatus,
};
use crate::workspace_git::open_workspace_validator_ledger;
use crate::workspace_policy::{
    Validator, ValidatorPhaseKind, WorkspacePolicy, WorkspacePolicyKind, WorkspaceSpawnTaskPolicy,
    read_workspace_policy,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnTaskContractResult {
    NotConfigured {
        required: bool,
        reason: Option<String>,
    },
    Satisfied {
        output_files: Vec<String>,
    },
    Failed {
        error: String,
        notify_user: Option<String>,
    },
}

#[derive(Debug, Clone)]
struct ResolvedArtifacts {
    context: ActionContext,
    paths: Vec<PathBuf>,
}

pub async fn enforce_spawn_task_contract(
    tools: &ToolRegistry,
    tool_name: &str,
    tool_call_id: &str,
    files_to_send: &[PathBuf],
    task_started_at: SystemTime,
    supervisor: Option<(&TaskSupervisor, &str)>,
) -> SpawnTaskContractResult {
    let required_by_default = default_session_policy_requires_contract(tool_name);
    let Some(workspace_root) = tools.workspace_root() else {
        return SpawnTaskContractResult::NotConfigured {
            required: required_by_default,
            reason: required_by_default.then_some("workspace root unavailable".into()),
        };
    };

    let policy = match read_workspace_policy(workspace_root) {
        Ok(Some(policy)) => policy,
        Ok(None) => {
            return SpawnTaskContractResult::NotConfigured {
                required: required_by_default,
                reason: required_by_default.then_some("workspace policy not found".into()),
            };
        }
        Err(error) => {
            return SpawnTaskContractResult::Failed {
                error: format!("workspace contract read failed: {error}"),
                notify_user: None,
            };
        }
    };

    let Some(task_policy) = policy.spawn_tasks.get(tool_name).cloned() else {
        let required = policy.workspace.kind == WorkspacePolicyKind::Session && required_by_default;
        return SpawnTaskContractResult::NotConfigured {
            required,
            reason: required.then_some(format!(
                "workspace policy is missing spawn_tasks.{tool_name}"
            )),
        };
    };

    let notify_user = extract_notify_user(&task_policy);

    set_runtime_state(
        supervisor,
        TaskRuntimeState::ResolvingOutputs,
        Some(format!("resolve outputs for {tool_name}")),
    );
    let resolved_artifacts = match resolve_artifacts(
        workspace_root,
        &policy,
        &task_policy,
        files_to_send,
        task_started_at,
    ) {
        Ok(resolved) => resolved,
        Err(error) => {
            run_failure_actions(workspace_root, supervisor, &task_policy.on_failure, None);
            return SpawnTaskContractResult::Failed { error, notify_user };
        }
    };

    set_runtime_state(
        supervisor,
        TaskRuntimeState::VerifyingOutputs,
        Some(format!("verify outputs for {tool_name}")),
    );
    if let Err(error) =
        run_verify_actions(workspace_root, &task_policy.on_verify, &resolved_artifacts)
    {
        run_failure_actions(
            workspace_root,
            supervisor,
            &task_policy.on_failure,
            Some(&resolved_artifacts),
        );
        return SpawnTaskContractResult::Failed { error, notify_user };
    }

    // Run declarative validators (harness M4.3). Required failures block
    // terminal success via the same gating pathway as a missing-artifact
    // failure above — we treat a required validator failure as a hard contract
    // error and return Failed without entering the delivery phase. Optional
    // failures surface as warning counters through the ledger.
    match run_declared_validators(
        tools,
        workspace_root,
        &policy.validation.validators,
        tool_name,
        ValidatorPhase::Completion,
    )
    .await
    {
        Ok(_) => {}
        Err(error) => {
            run_failure_actions(
                workspace_root,
                supervisor,
                &task_policy.on_failure,
                Some(&resolved_artifacts),
            );
            return SpawnTaskContractResult::Failed { error, notify_user };
        }
    }

    set_runtime_state(
        supervisor,
        TaskRuntimeState::DeliveringOutputs,
        Some(format!("handoff outputs for {tool_name}")),
    );
    if task_policy.delivery_actions().is_empty() {
        let output_files = resolved_artifacts
            .paths
            .iter()
            .map(|path| path.to_string_lossy().to_string())
            .collect();
        return SpawnTaskContractResult::Satisfied { output_files };
    }

    match run_delivery_actions(
        tools,
        workspace_root,
        tool_call_id,
        task_policy.delivery_actions(),
        &resolved_artifacts,
    )
    .await
    {
        Ok(output_files) => SpawnTaskContractResult::Satisfied { output_files },
        Err(error) => {
            run_failure_actions(
                workspace_root,
                supervisor,
                &task_policy.on_failure,
                Some(&resolved_artifacts),
            );
            SpawnTaskContractResult::Failed { error, notify_user }
        }
    }
}

fn resolve_artifacts(
    workspace_root: &Path,
    policy: &WorkspacePolicy,
    task_policy: &WorkspaceSpawnTaskPolicy,
    files_to_send: &[PathBuf],
    task_started_at: SystemTime,
) -> Result<ResolvedArtifacts, String> {
    if !files_to_send.is_empty() {
        let files: Vec<PathBuf> = files_to_send
            .iter()
            .filter(|path| path.exists())
            .cloned()
            .collect();
        if files.is_empty() {
            return Err(
                "contract expected output files but tool-reported files do not exist".into(),
            );
        }
        let context = bind_explicit_files_to_artifacts(task_policy, files.clone())?;
        return Ok(ResolvedArtifacts {
            context,
            paths: files,
        });
    }

    let artifact_sources = task_policy.artifact_sources();
    if artifact_sources.is_empty() {
        return Err("workspace contract has no artifact source".into());
    }

    let mut context = ActionContext::default();
    let mut artifact_paths = Vec::new();

    for artifact_name in artifact_sources {
        let pattern = policy.artifacts.entries.get(artifact_name).ok_or_else(|| {
            format!("workspace contract references unknown artifact '{artifact_name}'")
        })?;

        let mut matches = resolve_glob_matches(workspace_root, pattern, Some(task_started_at))?;
        if matches.is_empty() {
            matches = resolve_glob_matches(workspace_root, pattern, None)?;
        }
        if matches.is_empty() {
            return Err(format!(
                "contract could not find artifact '{artifact_name}' matching '{pattern}'"
            ));
        }

        context = context.with_named_target(format!("${artifact_name}"), matches.clone());
        artifact_paths.extend(matches);
    }

    context = context.with_named_target("$artifact", artifact_paths.clone());

    Ok(ResolvedArtifacts {
        context,
        paths: artifact_paths,
    })
}

fn bind_explicit_files_to_artifacts(
    task_policy: &WorkspaceSpawnTaskPolicy,
    files: Vec<PathBuf>,
) -> Result<ActionContext, String> {
    let artifact_sources = task_policy.artifact_sources();
    if artifact_sources.is_empty() {
        return Err("workspace contract has no artifact source".into());
    }

    let mut context = ActionContext::default();
    if artifact_sources.len() == 1 {
        context = context.with_named_target(format!("${}", artifact_sources[0]), files.clone());
    } else {
        if artifact_sources.len() != files.len() {
            return Err(format!(
                "workspace contract expects {} explicit output files for {:?}, got {}",
                artifact_sources.len(),
                artifact_sources,
                files.len()
            ));
        }

        for (artifact_name, path) in artifact_sources.iter().zip(files.iter()) {
            context = context.with_named_target(format!("${artifact_name}"), vec![path.clone()]);
        }
    }

    Ok(context.with_named_target("$artifact", files))
}

fn resolve_glob_matches(
    workspace_root: &Path,
    pattern: &str,
    started_at: Option<SystemTime>,
) -> Result<Vec<PathBuf>, String> {
    let absolute_pattern = if Path::new(pattern).is_absolute() {
        PathBuf::from(pattern)
    } else {
        workspace_root.join(pattern)
    };

    let threshold = started_at
        .and_then(|value| value.checked_sub(Duration::from_secs(2)))
        .unwrap_or(UNIX_EPOCH);

    let mut matches = Vec::new();
    for entry in glob(&absolute_pattern.to_string_lossy())
        .map_err(|error| format!("invalid artifact glob '{pattern}': {error}"))?
    {
        let path =
            entry.map_err(|error| format!("artifact glob failed for '{pattern}': {error}"))?;
        if !path.is_file() {
            continue;
        }
        if started_at.is_some() {
            let modified = std::fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .unwrap_or(UNIX_EPOCH);
            if modified < threshold {
                continue;
            }
        }
        matches.push(path);
    }
    matches.sort_by(|left, right| {
        let left_modified = std::fs::metadata(left)
            .and_then(|meta| meta.modified())
            .unwrap_or(UNIX_EPOCH);
        let right_modified = std::fs::metadata(right)
            .and_then(|meta| meta.modified())
            .unwrap_or(UNIX_EPOCH);
        right_modified.cmp(&left_modified)
    });
    Ok(matches)
}

fn run_verify_actions(
    workspace_root: &Path,
    actions: &[String],
    resolved_artifacts: &ResolvedArtifacts,
) -> Result<(), String> {
    let mut failures = Vec::new();
    for (spec, result) in
        evaluate_actions_with_context(workspace_root, &resolved_artifacts.context, actions)
    {
        match result {
            Ok(ActionResult::Pass | ActionResult::Notify { .. }) => {}
            Ok(ActionResult::Fail { reason }) => failures.push(format!("{spec}: {reason}")),
            Err(error) => failures.push(format!("{spec}: validator error: {error}")),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

async fn run_delivery_actions(
    tools: &ToolRegistry,
    workspace_root: &Path,
    tool_call_id: &str,
    actions: &[String],
    resolved_artifacts: &ResolvedArtifacts,
) -> Result<Vec<String>, String> {
    let mut delivered_files = Vec::new();

    for action in actions {
        if let Some(target) = action.strip_prefix("send_file:") {
            for path in resolved_artifacts
                .context
                .resolve_targets(workspace_root, target)
                .map_err(|error| format!("send_file target resolution failed: {error}"))?
            {
                let path_str = path.to_string_lossy().to_string();
                let send_args =
                    serde_json::json!({ "file_path": path_str, "tool_call_id": tool_call_id });
                match tools.execute("send_file", &send_args).await {
                    Ok(result) if result.success => delivered_files.push(path_str),
                    Ok(result) => {
                        return Err(format!(
                            "send_file failed for {}: {}",
                            path.display(),
                            result.output
                        ));
                    }
                    Err(error) => {
                        return Err(format!("send_file failed for {}: {error}", path.display()));
                    }
                }
            }
            continue;
        }

        match run_action_with_context(workspace_root, &resolved_artifacts.context, action)
            .map_err(|error| format!("delivery action error: {error}"))?
        {
            ActionResult::Pass | ActionResult::Notify { .. } => continue,
            ActionResult::Fail { reason } => {
                return Err(format!("delivery action failed: {action}: {reason}"));
            }
        }
    }

    if delivered_files.is_empty() {
        Ok(resolved_artifacts
            .paths
            .iter()
            .map(|path| path.to_string_lossy().to_string())
            .collect())
    } else {
        Ok(delivered_files)
    }
}

fn run_failure_actions(
    workspace_root: &Path,
    supervisor: Option<(&TaskSupervisor, &str)>,
    actions: &[String],
    resolved_artifacts: Option<&ResolvedArtifacts>,
) {
    if actions.iter().any(|action| action.starts_with("cleanup:")) {
        set_runtime_state(
            supervisor,
            TaskRuntimeState::CleaningUp,
            Some("cleanup failed outputs".to_string()),
        );
    }
    let action_context = resolved_artifacts
        .map(|resolved| resolved.context.clone())
        .unwrap_or_default()
        .with_named_target(
            "$artifact",
            resolved_artifacts
                .map(|resolved| resolved.paths.clone())
                .unwrap_or_default(),
        );
    for action in actions {
        let _ = run_action_with_context(workspace_root, &action_context, action);
    }
}

fn extract_notify_user(task_policy: &WorkspaceSpawnTaskPolicy) -> Option<String> {
    task_policy
        .on_failure
        .iter()
        .find_map(|action| action.strip_prefix("notify_user:").map(ToOwned::to_owned))
}

fn set_runtime_state(
    supervisor: Option<(&TaskSupervisor, &str)>,
    runtime_state: TaskRuntimeState,
    detail: Option<String>,
) {
    if let Some((supervisor, task_id)) = supervisor {
        supervisor.mark_runtime_state(task_id, runtime_state, detail);
    }
}

fn default_session_policy_requires_contract(tool_name: &str) -> bool {
    WorkspacePolicy::for_session()
        .spawn_tasks
        .contains_key(tool_name)
}

/// Run declared typed validators for a workspace contract gate.
///
/// Persists every outcome to the workspace ledger (for replay). Returns
/// `Err(reason)` if any required validator fails — the caller treats this as
/// a contract-gate failure, matching the behaviour of a missing declared
/// artifact.
pub async fn run_declared_validators(
    tools: &ToolRegistry,
    workspace_root: &Path,
    validators: &[Validator],
    repo_label_hint: &str,
    phase: ValidatorPhase,
) -> Result<Vec<ValidatorOutcome>, String> {
    if validators.is_empty() {
        return Ok(Vec::new());
    }

    let scoped: Vec<Validator> = validators
        .iter()
        .filter(|v| match phase {
            ValidatorPhase::TurnEnd => v.phase == ValidatorPhaseKind::TurnEnd,
            ValidatorPhase::Completion => v.phase == ValidatorPhaseKind::Completion,
        })
        .cloned()
        .collect();
    if scoped.is_empty() {
        return Ok(Vec::new());
    }

    let ledger = match open_workspace_validator_ledger(workspace_root) {
        Ok(ledger) => Some(ledger),
        Err(err) => {
            tracing::warn!(
                workspace = %workspace_root.display(),
                error = %err,
                "failed to open validator ledger; continuing without replay persistence"
            );
            None
        }
    };

    let runner = build_validator_runner(tools, workspace_root);
    let runner = match ledger {
        Some(ledger) => runner.with_ledger(ledger),
        None => runner,
    };

    let invocation = ValidatorInvocation {
        phase,
        workspace_root: workspace_root.to_path_buf(),
        repo_label: repo_label_hint.to_string(),
    };

    let outcomes = runner.run_all(&invocation, &scoped).await;
    let failures: Vec<&ValidatorOutcome> = outcomes
        .iter()
        .filter(|o| o.required && o.status != ValidatorStatus::Pass)
        .collect();
    if !failures.is_empty() {
        let joined = failures
            .iter()
            .map(|o| format!("{}: {}", o.validator_id, o.reason))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!("required validator failure: {joined}"));
    }
    Ok(outcomes)
}

fn build_validator_runner(tools: &ToolRegistry, workspace_root: &Path) -> ValidatorRunner {
    // Capture a lightweight snapshot of tool handles for the validator runner.
    // Avoids cloning the full registry and its LRU bookkeeping.
    let dispatcher: Arc<dyn crate::validators::ValidatorToolDispatcher> =
        Arc::new(crate::validators::MapToolDispatcher::from_registry(tools));
    ValidatorRunner::with_dispatcher(dispatcher, workspace_root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    use crate::{Tool, ToolRegistry, ToolResult, WorkspacePolicy, write_workspace_policy};

    #[derive(Clone, Default)]
    struct CaptureSendFileTool {
        calls: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl Tool for CaptureSendFileTool {
        fn name(&self) -> &str {
            "send_file"
        }

        fn description(&self) -> &str {
            "capture send_file calls"
        }

        fn input_schema(&self) -> serde_json::Value {
            json!({
                "type": "object",
                "properties": {
                    "file_path": { "type": "string" },
                    "tool_call_id": { "type": "string" }
                },
                "required": ["file_path", "tool_call_id"]
            })
        }

        async fn execute(&self, args: &serde_json::Value) -> eyre::Result<ToolResult> {
            let file_path = args
                .get("file_path")
                .and_then(|value| value.as_str())
                .expect("send_file should receive a file_path")
                .to_string();
            self.calls.lock().unwrap().push(file_path);
            Ok(ToolResult {
                success: true,
                output: "sent".into(),
                ..Default::default()
            })
        }
    }

    #[tokio::test]
    async fn tts_contract_resolves_new_mp3_for_actor_delivery() {
        let temp = tempfile::tempdir().unwrap();
        write_workspace_policy(temp.path(), &WorkspacePolicy::for_session()).unwrap();
        let output = temp.path().join("tts_result.mp3");
        std::fs::write(&output, vec![1u8; 2048]).unwrap();

        let result = enforce_spawn_task_contract(
            &ToolRegistry::with_builtins(temp.path()),
            "fm_tts",
            "tool-call-1",
            &[],
            UNIX_EPOCH,
            None,
        )
        .await;

        match result {
            SpawnTaskContractResult::Satisfied { output_files } => {
                assert_eq!(output_files, vec![output.to_string_lossy().to_string()]);
            }
            other => panic!("expected success, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tts_contract_fails_when_no_mp3_exists() {
        let temp = tempfile::tempdir().unwrap();
        write_workspace_policy(temp.path(), &WorkspacePolicy::for_session()).unwrap();

        let result = enforce_spawn_task_contract(
            &ToolRegistry::with_builtins(temp.path()),
            "fm_tts",
            "tool-call-2",
            &[],
            UNIX_EPOCH,
            None,
        )
        .await;

        match result {
            SpawnTaskContractResult::Failed { error, notify_user } => {
                assert!(error.contains("artifact"));
                assert_eq!(notify_user.as_deref(), Some("TTS generation failed"));
            }
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn podcast_contract_resolves_generated_audio_for_actor_delivery() {
        let temp = tempfile::tempdir().unwrap();
        write_workspace_policy(temp.path(), &WorkspacePolicy::for_session()).unwrap();
        let output = temp
            .path()
            .join("skill-output/mofa-podcast/podcast_full_123.wav");
        std::fs::create_dir_all(output.parent().unwrap()).unwrap();
        std::fs::write(&output, vec![1u8; 8192]).unwrap();

        let result = enforce_spawn_task_contract(
            &ToolRegistry::with_builtins(temp.path()),
            "podcast_generate",
            "tool-call-3",
            std::slice::from_ref(&output),
            UNIX_EPOCH,
            None,
        )
        .await;

        match result {
            SpawnTaskContractResult::Satisfied { output_files } => {
                assert_eq!(output_files, vec![output.to_string_lossy().to_string()]);
            }
            other => panic!("expected success, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn session_contract_resolves_multiple_artifact_sources_for_runtime_verification() {
        let temp = tempfile::tempdir().unwrap();
        let mut policy = WorkspacePolicy::for_session();
        policy
            .artifacts
            .entries
            .insert("report".into(), "report.md".into());
        policy
            .artifacts
            .entries
            .insert("audio".into(), "audio.mp3".into());
        policy.spawn_tasks.insert(
            "bundle_generate".into(),
            WorkspaceSpawnTaskPolicy {
                artifact: None,
                artifacts: vec!["report".into(), "audio".into()],
                on_verify: vec![
                    "file_exists:$report".into(),
                    "file_exists:$audio".into(),
                    "file_size_min:$audio:1024".into(),
                ],
                on_complete: Vec::new(),
                on_deliver: Vec::new(),
                on_failure: Vec::new(),
            },
        );
        write_workspace_policy(temp.path(), &policy).unwrap();

        let report = temp.path().join("report.md");
        let audio = temp.path().join("audio.mp3");
        std::fs::write(&report, b"report").unwrap();
        std::fs::write(&audio, vec![0u8; 2048]).unwrap();

        let result = enforce_spawn_task_contract(
            &ToolRegistry::with_builtins(temp.path()),
            "bundle_generate",
            "tool-call-4",
            &[],
            UNIX_EPOCH,
            None,
        )
        .await;

        match result {
            SpawnTaskContractResult::Satisfied { output_files } => {
                assert_eq!(
                    output_files,
                    vec![
                        report.to_string_lossy().to_string(),
                        audio.to_string_lossy().to_string(),
                    ]
                );
            }
            other => panic!("expected success, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn session_contract_prefers_explicit_delivery_actions_over_legacy_completion_actions() {
        let temp = tempfile::tempdir().unwrap();
        let bundle = temp.path().join("bundle.md");
        std::fs::write(&bundle, b"bundle").unwrap();

        let mut policy = WorkspacePolicy::for_session();
        policy
            .artifacts
            .entries
            .insert("bundle".into(), "bundle.md".into());
        policy.spawn_tasks.insert(
            "bundle_generate".into(),
            WorkspaceSpawnTaskPolicy {
                artifact: Some("bundle".into()),
                artifacts: vec!["bundle".into()],
                on_verify: vec!["file_exists:$bundle".into()],
                on_complete: vec!["file_exists:missing.txt".into()],
                on_deliver: vec!["notify_user:bundle delivered".into()],
                on_failure: Vec::new(),
            },
        );
        write_workspace_policy(temp.path(), &policy).unwrap();

        let result = enforce_spawn_task_contract(
            &ToolRegistry::with_builtins(temp.path()),
            "bundle_generate",
            "tool-call-4",
            &[],
            UNIX_EPOCH,
            None,
        )
        .await;

        match result {
            SpawnTaskContractResult::Satisfied { output_files } => {
                assert_eq!(output_files, vec![bundle.to_string_lossy().to_string()]);
            }
            other => panic!("expected success, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn session_contract_binds_explicit_files_to_named_artifacts_for_delivery_actions() {
        let temp = tempfile::tempdir().unwrap();
        let report = temp.path().join("report.md");
        let audio = temp.path().join("audio.mp3");
        std::fs::write(&report, b"report").unwrap();
        std::fs::write(&audio, vec![0u8; 2048]).unwrap();

        let mut policy = WorkspacePolicy::for_session();
        policy
            .artifacts
            .entries
            .insert("report".into(), "report.md".into());
        policy
            .artifacts
            .entries
            .insert("audio".into(), "audio.mp3".into());
        policy.spawn_tasks.insert(
            "bundle_generate".into(),
            WorkspaceSpawnTaskPolicy {
                artifact: Some("legacy".into()),
                artifacts: vec!["report".into(), "audio".into()],
                on_verify: vec![
                    "file_exists:$report".into(),
                    "file_exists:$audio".into(),
                    "file_size_min:$audio:1024".into(),
                ],
                on_complete: vec!["send_file:$legacy".into()],
                on_deliver: vec!["send_file:$report".into(), "send_file:$audio".into()],
                on_failure: Vec::new(),
            },
        );
        write_workspace_policy(temp.path(), &policy).unwrap();

        let capture = CaptureSendFileTool::default();
        let calls = capture.calls.clone();
        let mut registry = ToolRegistry::with_builtins(temp.path());
        registry.register(capture);

        let result = enforce_spawn_task_contract(
            &registry,
            "bundle_generate",
            "tool-call-5",
            &[report.clone(), audio.clone()],
            UNIX_EPOCH,
            None,
        )
        .await;

        match result {
            SpawnTaskContractResult::Satisfied { output_files } => {
                assert_eq!(
                    output_files,
                    vec![
                        report.to_string_lossy().to_string(),
                        audio.to_string_lossy().to_string(),
                    ]
                );
            }
            other => panic!("expected success, got {other:?}"),
        }

        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                report.to_string_lossy().to_string(),
                audio.to_string_lossy().to_string()
            ]
        );
    }

    #[tokio::test]
    async fn session_contract_rejects_mismatched_explicit_output_counts() {
        let temp = tempfile::tempdir().unwrap();
        let report = temp.path().join("report.md");
        std::fs::write(&report, b"report").unwrap();

        let mut policy = WorkspacePolicy::for_session();
        policy
            .artifacts
            .entries
            .insert("report".into(), "report.md".into());
        policy
            .artifacts
            .entries
            .insert("audio".into(), "audio.mp3".into());
        policy.spawn_tasks.insert(
            "bundle_generate".into(),
            WorkspaceSpawnTaskPolicy {
                artifact: None,
                artifacts: vec!["report".into(), "audio".into()],
                on_verify: Vec::new(),
                on_complete: Vec::new(),
                on_deliver: Vec::new(),
                on_failure: Vec::new(),
            },
        );
        write_workspace_policy(temp.path(), &policy).unwrap();

        let result = enforce_spawn_task_contract(
            &ToolRegistry::with_builtins(temp.path()),
            "bundle_generate",
            "tool-call-6",
            std::slice::from_ref(&report),
            UNIX_EPOCH,
            None,
        )
        .await;

        match result {
            SpawnTaskContractResult::Failed { error, .. } => {
                assert!(error.contains("expects 2 explicit output files"));
            }
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn session_tts_contract_is_required_when_policy_file_is_missing() {
        let temp = tempfile::tempdir().unwrap();

        let result = enforce_spawn_task_contract(
            &ToolRegistry::with_builtins(temp.path()),
            "fm_tts",
            "tool-call-4",
            &[],
            UNIX_EPOCH,
            None,
        )
        .await;

        assert_eq!(
            result,
            SpawnTaskContractResult::NotConfigured {
                required: true,
                reason: Some("workspace policy not found".into()),
            }
        );
    }

    #[tokio::test]
    async fn unrelated_spawn_tool_without_contract_is_not_required() {
        let temp = tempfile::tempdir().unwrap();

        let result = enforce_spawn_task_contract(
            &ToolRegistry::with_builtins(temp.path()),
            "unknown_background_tool",
            "tool-call-5",
            &[],
            UNIX_EPOCH,
            None,
        )
        .await;

        assert_eq!(
            result,
            SpawnTaskContractResult::NotConfigured {
                required: false,
                reason: None,
            }
        );
    }

    #[test]
    fn runtime_verification_uses_shared_validator_semantics_for_file_size_checks() {
        let temp = tempfile::tempdir().unwrap();
        let artifact = temp.path().join("output.mp3");
        std::fs::write(&artifact, b"x").unwrap();

        let resolved_artifacts = ResolvedArtifacts {
            context: ActionContext::default()
                .with_named_target("$artifact", vec![artifact.clone()]),
            paths: vec![artifact.clone()],
        };

        let error = run_verify_actions(
            temp.path(),
            &["file_size_min:$artifact:1024".into()],
            &resolved_artifacts,
        )
        .unwrap_err();

        assert!(error.contains("file_size_min:$artifact:1024"));
        assert!(error.contains("output.mp3 is 1 bytes, minimum is 1024"));
    }
}
