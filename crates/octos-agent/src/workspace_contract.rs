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
use crate::workspace_git::{
    WorkspaceProjectKind, list_workspace_repos, open_workspace_validator_ledger,
};
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
    enforce_spawn_task_contract_with_args_and_output(
        tools,
        tool_name,
        tool_call_id,
        files_to_send,
        task_started_at,
        supervisor,
        None,
        None,
    )
    .await
}

/// Variant of [`enforce_spawn_task_contract`] that threads the originating
/// spawn task's input args so domain validators (`HttpProbe`,
/// `OminixVoiceExists`) can resolve `${args.<key>}` references against them.
///
/// Production callers in the agent loop should prefer
/// [`enforce_spawn_task_contract_with_args_and_output`] (which also threads
/// the tool's `named_outputs` for `${output.<key>}` interpolation). This
/// entry point exists for callers that have args but no tool output to
/// forward.
pub async fn enforce_spawn_task_contract_with_args(
    tools: &ToolRegistry,
    tool_name: &str,
    tool_call_id: &str,
    files_to_send: &[PathBuf],
    task_started_at: SystemTime,
    supervisor: Option<(&TaskSupervisor, &str)>,
    input_args: Option<&serde_json::Value>,
) -> SpawnTaskContractResult {
    enforce_spawn_task_contract_with_args_and_output(
        tools,
        tool_name,
        tool_call_id,
        files_to_send,
        task_started_at,
        supervisor,
        input_args,
        None,
    )
    .await
}

/// Full variant of [`enforce_spawn_task_contract`] that threads BOTH the
/// originating spawn task's input args (for `${args.<key>}` interpolation)
/// AND the tool's `named_outputs` (for `${output.<key>}` interpolation).
///
/// `tool_named_outputs` is a JSON object built from the tool's stdout
/// envelope; pass `None` for tools that emit nothing. The contract layer
/// forwards it verbatim to [`run_declared_validators_with_output`] so the
/// validator runner can interpolate templated URLs (e.g. `mofa_publish`
/// emitting `deploy_url` then `HttpProbe { url_template = "${output.deploy_url}" }`).
#[allow(clippy::too_many_arguments)]
pub async fn enforce_spawn_task_contract_with_args_and_output(
    tools: &ToolRegistry,
    tool_name: &str,
    tool_call_id: &str,
    files_to_send: &[PathBuf],
    task_started_at: SystemTime,
    supervisor: Option<(&TaskSupervisor, &str)>,
    input_args: Option<&serde_json::Value>,
    tool_named_outputs: Option<&serde_json::Value>,
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
    //
    // Merge workspace-wide validators with the per-spawn-task
    // `on_completion` list so domain validators declared inline next to the
    // spawn task contract run in the same gate.
    let mut combined_validators: Vec<Validator> = policy.validation.validators.clone();
    for (index, entry) in task_policy.on_completion.iter().enumerate() {
        combined_validators.push(entry.clone().into_validator(tool_name, index));
    }
    match run_declared_validators_with_output(
        tools,
        workspace_root,
        &combined_validators,
        tool_name,
        ValidatorPhase::Completion,
        input_args.cloned(),
        tool_named_outputs.cloned(),
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
        // Contract declares no artifact-source — this is allowed for
        // spawn tasks that produce no on-disk file (e.g. `fm_voice_save`
        // which mutates an external API). Skip artifact resolution and
        // hand the validator runner an empty resolved context; typed
        // validators in `on_completion` will still run.
        if task_policy.on_verify.is_empty() && task_policy.delivery_actions().is_empty() {
            return Ok(ResolvedArtifacts {
                context: ActionContext::default(),
                paths: Vec::new(),
            });
        }
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
///
/// `input_args` carries the originating spawn task's input JSON so that
/// domain validators (`HttpProbe`, `OminixVoiceExists`) can resolve
/// `${args.<key>}` references. Pass `None` for non-spawn contexts (e.g.
/// turn-end validators that don't reference task inputs).
///
/// Thin wrapper for non-spawn-only callers that have no tool output to
/// forward. Spawn-only callers should use [`run_declared_validators_with_output`].
pub async fn run_declared_validators(
    tools: &ToolRegistry,
    workspace_root: &Path,
    validators: &[Validator],
    repo_label_hint: &str,
    phase: ValidatorPhase,
    input_args: Option<serde_json::Value>,
) -> Result<Vec<ValidatorOutcome>, String> {
    run_declared_validators_with_output(
        tools,
        workspace_root,
        validators,
        repo_label_hint,
        phase,
        input_args,
        None,
    )
    .await
}

/// Variant of [`run_declared_validators`] that also threads the spawn
/// task's `named_outputs` (`tool_output`) into the validator invocation so
/// domain validators can resolve `${output.<key>}` references against
/// tool-emitted values (e.g. `mofa_publish` emitting `deploy_url` for the
/// HttpProbe to call).
pub async fn run_declared_validators_with_output(
    tools: &ToolRegistry,
    workspace_root: &Path,
    validators: &[Validator],
    repo_label_hint: &str,
    phase: ValidatorPhase,
    input_args: Option<serde_json::Value>,
    tool_output: Option<serde_json::Value>,
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
        input_args,
        tool_output,
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

/// octos #997 (round-2 fix): aggregated result for project-root validator runs.
///
/// `run_project_root_validators` iterates every policy-managed
/// slides/sites project beneath the session's `working_dir` and runs each
/// project's declared completion-phase validators AT THE PROJECT ROOT —
/// so the resulting ledger writes land at
/// `<working_dir>/<kind>/<slug>/.octos/validator_outcomes.jsonl`, which is
/// the exact path `inspect_workspace_contract` reads.
#[derive(Debug, Default, Clone)]
pub struct ProjectRootValidatorReport {
    /// Number of project roots beneath `working_dir` that had at least one
    /// declared validator and ran the validator chain (Pass or Fail).
    pub projects_run: usize,
    /// `(repo_label, reason)` for any project whose declared validator chain
    /// failed at its OWN project root. Callers should treat the first entry as
    /// the load-bearing failure reason that demotes the spawn contract.
    pub failures: Vec<(String, String)>,
}

impl ProjectRootValidatorReport {
    pub fn is_empty(&self) -> bool {
        self.projects_run == 0
    }

    pub fn first_failure_reason(&self) -> Option<String> {
        self.failures
            .first()
            .map(|(repo_label, reason)| format!("{repo_label}: {reason}"))
    }
}

/// octos #997 (round-2 fix): run each managed project's declared
/// completion-phase validators AT THE PROJECT ROOT.
///
/// The session-scope spawn-task contract calls
/// [`run_declared_validators`] with the SESSION root as `workspace_root`,
/// which is correct for session-scope policies (the validator ledger lives
/// under `<session>/.octos/validator_outcomes.jsonl`). But the
/// project-scope contract gate — `inspect_workspace_contract` —
/// reads `<session>/slides/<slug>/.octos/validator_outcomes.jsonl`. If
/// nobody writes to that path, a real valid deck whose declared validator
/// is hard-required (octos #997: `slides.mofa_slides.pptx_magic_bytes`)
/// shows `ready = false` because the persisted outcome is missing — even
/// though the artifact is genuinely on disk.
///
/// This helper closes the gap: for each slides/sites project beneath
/// `working_dir`, read the project's own `WorkspacePolicy` and invoke
/// [`run_declared_validators`] with that project root as `workspace_root`.
/// The resulting outcomes naturally land in the project ledger that
/// `inspect_workspace_contract` reads.
///
/// Returns a [`ProjectRootValidatorReport`] aggregating the per-project
/// outcomes. Callers that want to short-circuit the spawn contract on a
/// project-root validator failure should consult
/// [`ProjectRootValidatorReport::first_failure_reason`].
pub async fn run_project_root_validators(
    tools: &ToolRegistry,
    working_dir: &Path,
    expected_kind: Option<WorkspaceProjectKind>,
) -> ProjectRootValidatorReport {
    let mut report = ProjectRootValidatorReport::default();
    let repos = match list_workspace_repos(working_dir) {
        Ok(repos) => repos,
        Err(error) => {
            tracing::warn!(
                working_dir = %working_dir.display(),
                error = %error,
                "project-root validator: failed to list workspace repos"
            );
            return report;
        }
    };

    for repo in repos {
        if let Some(kind) = expected_kind {
            if repo.kind != kind {
                continue;
            }
        }
        let project_root = repo.root.clone();
        let repo_label = format!("{}/{}", repo.kind.directory_name(), repo.slug);

        let policy = match read_workspace_policy(&project_root) {
            Ok(Some(policy)) => policy,
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(
                    project_root = %project_root.display(),
                    error = %error,
                    "project-root validator: failed to read project policy"
                );
                continue;
            }
        };

        if policy.validation.validators.is_empty() {
            continue;
        }

        report.projects_run = report.projects_run.saturating_add(1);
        match run_declared_validators(
            tools,
            &project_root,
            &policy.validation.validators,
            &repo_label,
            ValidatorPhase::Completion,
            None,
        )
        .await
        {
            Ok(_) => {}
            Err(reason) => {
                report.failures.push((repo_label, reason));
            }
        }
    }

    report
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
        // The default session contract now declares MP3-specific
        // `magic_bytes` + `audio_non_silent` domain validators on
        // `podcast_generate`. This test only exercises the artifact-
        // resolution path, so we strip the per-task validators to focus
        // on the legacy contract semantics. Tests for the new validators
        // live in the inline `validators` module.
        let mut policy = WorkspacePolicy::for_session();
        if let Some(task) = policy.spawn_tasks.get_mut("podcast_generate") {
            task.on_completion.clear();
        }
        write_workspace_policy(temp.path(), &policy).unwrap();
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
                on_completion: Vec::new(),
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
                on_completion: Vec::new(),
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
                on_completion: Vec::new(),
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
                on_completion: Vec::new(),
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

    /// Smallest valid PNG (1x1 transparent pixel) — used to satisfy
    /// MagicBytes (Png) without pulling in an encoder dependency.
    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG signature
        0x00, 0x00, 0x00, 0x0D, b'I', b'H', b'D', b'R', // IHDR header
        0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, // width=1, height=1
        0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4, 0x89, // bit depth+color
        0x00, 0x00, 0x00, 0x0D, b'I', b'D', b'A', b'T', // IDAT chunk
        0x78, 0x9C, 0x62, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
        0x00, 0x00, 0x00, b'I', b'E', b'N', b'D', // IEND chunk
        0xAE, 0x42, 0x60, 0x82,
    ];

    #[tokio::test]
    async fn mofa_slides_contract_satisfies_when_pptx_is_present() {
        // P1-4: the default session policy for `mofa_slides` should
        // verify a PPTX with a valid ZIP signature is present.
        let temp = tempfile::tempdir().unwrap();
        write_workspace_policy(temp.path(), &WorkspacePolicy::for_session()).unwrap();
        let pptx = temp.path().join("output/deck.pptx");
        std::fs::create_dir_all(pptx.parent().unwrap()).unwrap();
        // PK\x03\x04 followed by enough padding so the magic-byte read
        // succeeds and the file is non-trivial.
        let mut bytes = vec![0x50, 0x4B, 0x03, 0x04];
        bytes.extend(std::iter::repeat_n(0u8, 256));
        std::fs::write(&pptx, &bytes).unwrap();

        let result = enforce_spawn_task_contract_with_args(
            &ToolRegistry::with_builtins(temp.path()),
            "mofa_slides",
            "tool-call-slides",
            &[],
            UNIX_EPOCH,
            None,
            Some(&json!({"out": "output/deck.pptx"})),
        )
        .await;

        match result {
            SpawnTaskContractResult::Satisfied { .. } => {}
            other => panic!("expected success, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mofa_slides_contract_fails_when_artifact_is_html_error_page() {
        // Catches the silent-failure path: tool wrote an HTML error page
        // in place of the PPTX. MagicBytes (Pptx) rejects it.
        let temp = tempfile::tempdir().unwrap();
        write_workspace_policy(temp.path(), &WorkspacePolicy::for_session()).unwrap();
        let pptx = temp.path().join("output/deck.pptx");
        std::fs::create_dir_all(pptx.parent().unwrap()).unwrap();
        std::fs::write(&pptx, b"<!DOCTYPE html>\n<html>Internal error</html>\n").unwrap();

        let result = enforce_spawn_task_contract_with_args(
            &ToolRegistry::with_builtins(temp.path()),
            "mofa_slides",
            "tool-call-slides-fail",
            &[],
            UNIX_EPOCH,
            None,
            Some(&json!({"out": "output/deck.pptx"})),
        )
        .await;

        match result {
            SpawnTaskContractResult::Failed { error, notify_user } => {
                assert!(
                    error.contains("magic_bytes") || error.contains("pptx"),
                    "unexpected error: {error}"
                );
                assert_eq!(notify_user.as_deref(), Some("Slide generation failed"));
            }
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mofa_cards_contract_satisfies_when_png_files_match_recursive_glob() {
        // P1-5: mofa_cards emits PNGs under a per-task card_dir.
        let temp = tempfile::tempdir().unwrap();
        write_workspace_policy(temp.path(), &WorkspacePolicy::for_session()).unwrap();
        let card_dir = temp.path().join("cards/abc");
        std::fs::create_dir_all(&card_dir).unwrap();
        std::fs::write(card_dir.join("a.png"), PNG_1X1).unwrap();

        let result = enforce_spawn_task_contract_with_args(
            &ToolRegistry::with_builtins(temp.path()),
            "mofa_cards",
            "tool-call-cards",
            &[],
            UNIX_EPOCH,
            None,
            Some(&json!({"card_dir": "cards/abc"})),
        )
        .await;

        match result {
            SpawnTaskContractResult::Satisfied { .. } => {}
            other => panic!("expected success, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mofa_comic_contract_uses_args_out_for_file_exists_and_magic_bytes() {
        // P1-5: mofa_comic has a required `out` arg pointing at a single
        // PNG file. Both FileExists and MagicBytes interpolate
        // `${args.out}` so they assert exactly the path the LLM declared.
        let temp = tempfile::tempdir().unwrap();
        write_workspace_policy(temp.path(), &WorkspacePolicy::for_session()).unwrap();
        let comic = temp.path().join("comic.png");
        std::fs::write(&comic, PNG_1X1).unwrap();
        // Pad to meet the 1024-byte min_bytes check on the FileExists.
        let mut padded = PNG_1X1.to_vec();
        padded.extend(std::iter::repeat_n(0u8, 2048));
        std::fs::write(&comic, &padded).unwrap();

        let result = enforce_spawn_task_contract_with_args(
            &ToolRegistry::with_builtins(temp.path()),
            "mofa_comic",
            "tool-call-comic",
            &[],
            UNIX_EPOCH,
            None,
            Some(&json!({"out": "comic.png"})),
        )
        .await;

        match result {
            SpawnTaskContractResult::Satisfied { .. } => {}
            other => panic!("expected success, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mofa_comic_contract_fails_when_args_out_file_is_missing() {
        let temp = tempfile::tempdir().unwrap();
        write_workspace_policy(temp.path(), &WorkspacePolicy::for_session()).unwrap();
        // Note: the file `comic.png` is never created — the LLM-declared
        // path doesn't exist, so FileExists with `${args.out}` should
        // fail at the contract gate.

        let result = enforce_spawn_task_contract_with_args(
            &ToolRegistry::with_builtins(temp.path()),
            "mofa_comic",
            "tool-call-comic-fail",
            &[],
            UNIX_EPOCH,
            None,
            Some(&json!({"out": "comic.png"})),
        )
        .await;

        match result {
            SpawnTaskContractResult::Failed { error, notify_user } => {
                assert!(
                    error.contains("does not exist") || error.contains("comic.png"),
                    "unexpected error: {error}"
                );
                assert_eq!(notify_user.as_deref(), Some("Comic generation failed"));
            }
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fm_voice_save_contract_file_exists_succeeds_when_voice_wav_present() {
        // P0-1: the `fm_voice_save` contract now asserts the voice WAV
        // landed at `voice_profiles/<name>.wav` via FileExists +
        // `${args.name}` interpolation, in addition to the
        // OminixVoiceExists API probe.
        let temp = tempfile::tempdir().unwrap();
        // Strip the OminixVoiceExists validator so this test focuses on
        // the new FileExists check. The OminixVoiceExists validator is
        // covered by validators::tests inside `validators.rs`.
        let mut policy = WorkspacePolicy::for_session();
        if let Some(task) = policy.spawn_tasks.get_mut("fm_voice_save") {
            task.on_completion.retain(|spec| {
                matches!(
                    spec,
                    crate::workspace_policy::SpawnTaskValidatorSpec::Bare(
                        crate::workspace_policy::ValidatorSpec::FileExists { .. }
                    )
                )
            });
        }
        write_workspace_policy(temp.path(), &policy).unwrap();
        std::fs::create_dir_all(temp.path().join("voice_profiles")).unwrap();
        std::fs::write(
            temp.path().join("voice_profiles/yangmi.wav"),
            vec![0u8; 4096],
        )
        .unwrap();

        let result = enforce_spawn_task_contract_with_args(
            &ToolRegistry::with_builtins(temp.path()),
            "fm_voice_save",
            "tool-call-voice-save",
            &[],
            UNIX_EPOCH,
            None,
            Some(&json!({"name": "yangmi", "audio_path": "/tmp/in.wav"})),
        )
        .await;

        match result {
            SpawnTaskContractResult::Satisfied { .. } => {}
            other => panic!("expected success, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fm_voice_save_contract_file_exists_fails_when_voice_wav_missing() {
        let temp = tempfile::tempdir().unwrap();
        let mut policy = WorkspacePolicy::for_session();
        if let Some(task) = policy.spawn_tasks.get_mut("fm_voice_save") {
            task.on_completion.retain(|spec| {
                matches!(
                    spec,
                    crate::workspace_policy::SpawnTaskValidatorSpec::Bare(
                        crate::workspace_policy::ValidatorSpec::FileExists { .. }
                    )
                )
            });
        }
        write_workspace_policy(temp.path(), &policy).unwrap();
        // Don't write the WAV — FileExists with `${args.name}` must fail.

        let result = enforce_spawn_task_contract_with_args(
            &ToolRegistry::with_builtins(temp.path()),
            "fm_voice_save",
            "tool-call-voice-save-fail",
            &[],
            UNIX_EPOCH,
            None,
            Some(&json!({"name": "no_such_voice"})),
        )
        .await;

        match result {
            SpawnTaskContractResult::Failed { error, notify_user } => {
                assert!(
                    error.contains("no_such_voice.wav") || error.contains("does not exist"),
                    "unexpected error: {error}"
                );
                assert_eq!(notify_user.as_deref(), Some("Voice registration failed"));
            }
            other => panic!("expected failure, got {other:?}"),
        }
    }

    // -------------------------------------------------------------------
    // Wave-3b: named_outputs end-to-end through enforce_spawn_task_contract.
    // -------------------------------------------------------------------

    /// Tiny synchronous HTTP server scripted via `responses`. Re-used from
    /// the validators test module to drive end-to-end probes.
    fn spawn_test_http_server(responses: Vec<&'static str>) -> String {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local_addr").to_string();
        std::thread::spawn(move || {
            for body in responses {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(_) => return,
                };
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(body.as_bytes());
                let _ = stream.flush();
            }
        });
        addr
    }

    /// Build a `mofa_publish` policy with the HttpProbe forced to
    /// `required = true` so the test gate fails on a missing
    /// named_output. Mirrors the eventual post-mofa-skills-follow-up
    /// state of the policy.
    fn mofa_publish_required_policy(url_template: &str) -> WorkspacePolicy {
        use crate::Validator;
        use crate::workspace_policy::{SpawnTaskValidatorSpec, ValidatorPhaseKind, ValidatorSpec};

        let mut policy = WorkspacePolicy::for_session();
        let publish = policy.spawn_tasks.entry("mofa_publish".into()).or_default();
        publish.on_failure = vec!["notify_user:Publish probe failed".into()];
        publish.on_completion = vec![SpawnTaskValidatorSpec::Full(Validator {
            id: "mofa_publish.deploy_url_probe".into(),
            required: true,
            soft_fail: false,
            timeout_ms: Some(2000),
            phase: ValidatorPhaseKind::Completion,
            spec: ValidatorSpec::HttpProbe {
                url_template: url_template.to_string(),
                expected_status: 200,
                expected_contains: Some("<!DOCTYPE".into()),
            },
        })];
        policy
    }

    #[tokio::test]
    async fn mofa_publish_contract_satisfies_when_named_outputs_deploy_url_serves_doctype() {
        // End-to-end: tool emits `named_outputs.deploy_url`; contract
        // probes that URL; server returns a 200 with `<!DOCTYPE` body.
        let response = "HTTP/1.1 200 OK\r\nContent-Length: 14\r\n\r\n<!DOCTYPE html>";
        let addr = spawn_test_http_server(vec![response]);
        let temp = tempfile::tempdir().unwrap();
        // Force the validator to `required = true` so a missing/failing
        // probe blocks the contract.
        write_workspace_policy(
            temp.path(),
            &mofa_publish_required_policy("${output.deploy_url}"),
        )
        .unwrap();

        let result = enforce_spawn_task_contract_with_args_and_output(
            &ToolRegistry::with_builtins(temp.path()),
            "mofa_publish",
            "tool-call-publish-ok",
            &[],
            UNIX_EPOCH,
            None,
            None,
            Some(&json!({"deploy_url": format!("http://{addr}/site")})),
        )
        .await;

        match result {
            SpawnTaskContractResult::Satisfied { .. } => {}
            other => panic!("expected success, got {other:?}"),
        }
    }

    // ---------------------------------------------------------------------
    // Wave-3a: end-to-end contract gate exercises the three new variants
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn enforce_spawn_task_contract_with_args_runs_sha256_match_via_interpolation() {
        // End-to-end probe through `enforce_spawn_task_contract_with_args`
        // for the new `Sha256Match` variant. Mirrors how `manage_skills`
        // would wire its manifest-declared hash through input args.
        let temp = tempfile::tempdir().unwrap();
        let bytes = b"manage_skills binary payload\n";
        let expected_hex = {
            use sha2::{Digest, Sha256};
            format!("{:x}", Sha256::digest(bytes))
        };

        let skill_dir = temp.path().join("skills/example");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let binary_path = skill_dir.join("main");
        std::fs::write(&binary_path, bytes).unwrap();

        // Sole spawn-task contract: Sha256Match resolves the expected hash
        // through args, and no artifact source / on_verify means the
        // contract gate only runs the typed validators.
        let mut policy = WorkspacePolicy::for_session();
        policy.spawn_tasks.insert(
            "manage_skills_test".into(),
            WorkspaceSpawnTaskPolicy {
                artifact: None,
                artifacts: Vec::new(),
                on_verify: Vec::new(),
                on_complete: Vec::new(),
                on_deliver: Vec::new(),
                on_failure: vec!["notify_user:skill install verification failed".into()],
                on_completion: vec![crate::workspace_policy::SpawnTaskValidatorSpec::Bare(
                    crate::workspace_policy::ValidatorSpec::Sha256Match {
                        glob: "skills/example/main".into(),
                        sha256: "${args.expected_sha256}".into(),
                    },
                )],
            },
        );
        write_workspace_policy(temp.path(), &policy).unwrap();

        let result = enforce_spawn_task_contract_with_args(
            &ToolRegistry::with_builtins(temp.path()),
            "manage_skills_test",
            "tool-call-sha-ok",
            &[],
            UNIX_EPOCH,
            None,
            Some(&json!({"expected_sha256": expected_hex.clone()})),
        )
        .await;

        match result {
            SpawnTaskContractResult::Satisfied { .. } => {}
            other => panic!("expected satisfied, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mofa_publish_contract_fails_when_probe_returns_soft_404_html() {
        // 200 OK with a body that lacks `<!DOCTYPE` (e.g. a JSON soft-404
        // wrapper) must fail the contract.
        let response = "HTTP/1.1 200 OK\r\nContent-Length: 18\r\n\r\n{\"error\":\"missing\"}";
        let addr = spawn_test_http_server(vec![response]);
        let temp = tempfile::tempdir().unwrap();
        write_workspace_policy(
            temp.path(),
            &mofa_publish_required_policy("${output.deploy_url}"),
        )
        .unwrap();

        let result = enforce_spawn_task_contract_with_args_and_output(
            &ToolRegistry::with_builtins(temp.path()),
            "mofa_publish",
            "tool-call-publish-soft-404",
            &[],
            UNIX_EPOCH,
            None,
            None,
            Some(&json!({"deploy_url": format!("http://{addr}/missing")})),
        )
        .await;

        match result {
            SpawnTaskContractResult::Failed { error, notify_user } => {
                assert!(
                    error.contains("<!DOCTYPE") || error.contains("did not contain"),
                    "unexpected error: {error}"
                );
                assert_eq!(notify_user.as_deref(), Some("Publish probe failed"));
            }
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn enforce_spawn_task_contract_with_args_fails_when_sha256_does_not_match() {
        let temp = tempfile::tempdir().unwrap();
        let skill_dir = temp.path().join("skills/example");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("main"), b"actual contents").unwrap();

        let mut policy = WorkspacePolicy::for_session();
        policy.spawn_tasks.insert(
            "manage_skills_test".into(),
            WorkspaceSpawnTaskPolicy {
                artifact: None,
                artifacts: Vec::new(),
                on_verify: Vec::new(),
                on_complete: Vec::new(),
                on_deliver: Vec::new(),
                on_failure: vec!["notify_user:install verification failed".into()],
                on_completion: vec![crate::workspace_policy::SpawnTaskValidatorSpec::Bare(
                    crate::workspace_policy::ValidatorSpec::Sha256Match {
                        glob: "skills/example/main".into(),
                        sha256: "${args.expected_sha256}".into(),
                    },
                )],
            },
        );
        write_workspace_policy(temp.path(), &policy).unwrap();

        let wrong_hex = "f".repeat(64);
        let result = enforce_spawn_task_contract_with_args(
            &ToolRegistry::with_builtins(temp.path()),
            "manage_skills_test",
            "tool-call-sha-fail",
            &[],
            UNIX_EPOCH,
            None,
            Some(&json!({"expected_sha256": wrong_hex})),
        )
        .await;

        match result {
            SpawnTaskContractResult::Failed { error, notify_user } => {
                assert!(
                    error.contains("sha256_match") || error.contains("expected="),
                    "expected sha256 mismatch error, got: {error}"
                );
                assert_eq!(notify_user.as_deref(), Some("install verification failed"));
            }
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mofa_publish_contract_fails_when_named_outputs_deploy_url_missing() {
        // The skill claimed success but emitted NO named_outputs.
        // With a `required = true` probe, the contract should reject
        // the result because `${output.deploy_url}` is unresolvable.
        let temp = tempfile::tempdir().unwrap();
        write_workspace_policy(
            temp.path(),
            &mofa_publish_required_policy("${output.deploy_url}"),
        )
        .unwrap();

        let result = enforce_spawn_task_contract_with_args_and_output(
            &ToolRegistry::with_builtins(temp.path()),
            "mofa_publish",
            "tool-call-publish-missing-url",
            &[],
            UNIX_EPOCH,
            None,
            None,
            // tool_output absent — emulates the current mofa_publish
            // skill (before the mofa-skills repo follow-up).
            None,
        )
        .await;

        match result {
            SpawnTaskContractResult::Failed { error, .. } => {
                assert!(
                    error.contains("deploy_url"),
                    "error should name the missing output key: {error}"
                );
            }
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mofa_publish_default_contract_does_not_block_when_skill_not_yet_emitting_output() {
        // Until the mofa-skills repo follow-up lands, mofa_publish does
        // NOT yet emit `named_outputs.deploy_url`. The default contract
        // ships the probe as `required = false` so the missing key
        // produces a diagnostic ledger entry but does NOT fail the gate.
        let temp = tempfile::tempdir().unwrap();
        write_workspace_policy(temp.path(), &WorkspacePolicy::for_session()).unwrap();

        let result = enforce_spawn_task_contract_with_args_and_output(
            &ToolRegistry::with_builtins(temp.path()),
            "mofa_publish",
            "tool-call-publish-default-policy",
            &[],
            UNIX_EPOCH,
            None,
            None,
            // No named_outputs from the skill (current state).
            None,
        )
        .await;

        match result {
            SpawnTaskContractResult::Satisfied { .. } => {}
            other => panic!(
                "default mofa_publish policy must not block users until mofa-skills \
                 catches up; got {other:?}"
            ),
        }
    }

    #[tokio::test]
    async fn spawn_only_envelope_named_outputs_threads_through_contract_to_validator() {
        // Wave-3b protocol invariant: a spawn_only tool emits
        // `named_outputs` on stdout → the contract forwards it to the
        // validator runner → the runner uses `${output.X}` interpolation
        // to drive (in this case) a FileExists check against a tool-
        // emitted path. Validates the full chain end-to-end without
        // depending on HTTP.
        use crate::Validator;
        use crate::workspace_policy::{
            SpawnTaskValidatorSpec, ValidatorPhaseKind, ValidatorSpec, WorkspaceSpawnTaskPolicy,
        };

        let temp = tempfile::tempdir().unwrap();
        let mut policy = WorkspacePolicy::for_session();
        policy.spawn_tasks.insert(
            "fake_publish".into(),
            WorkspaceSpawnTaskPolicy {
                artifact: None,
                artifacts: Vec::new(),
                on_verify: Vec::new(),
                on_complete: vec![],
                on_deliver: vec![],
                on_failure: vec!["notify_user:Fake publish failed".into()],
                on_completion: vec![SpawnTaskValidatorSpec::Full(Validator {
                    id: "fake_publish.target_exists".into(),
                    required: true,
                    soft_fail: false,
                    timeout_ms: None,
                    phase: ValidatorPhaseKind::Completion,
                    spec: ValidatorSpec::FileExists {
                        path: "${output.target_path}".into(),
                        min_bytes: None,
                    },
                })],
            },
        );
        write_workspace_policy(temp.path(), &policy).unwrap();
        std::fs::write(temp.path().join("artifact.txt"), b"x").unwrap();

        let result = enforce_spawn_task_contract_with_args_and_output(
            &ToolRegistry::with_builtins(temp.path()),
            "fake_publish",
            "tool-call-fake",
            &[],
            UNIX_EPOCH,
            None,
            None,
            Some(&json!({"target_path": "artifact.txt"})),
        )
        .await;

        match result {
            SpawnTaskContractResult::Satisfied { .. } => {}
            other => panic!("expected named_outputs path to satisfy contract, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn enforce_spawn_task_contract_with_args_treats_soft_fail_validator_as_warning() {
        // Wire-target end-to-end probe for `Required::Soft`: a hard-required
        // validator that's also `soft_fail` MUST surface a Fail outcome to
        // the ledger but NOT demote the spawn task. Mirrors the
        // `synthesize_research`/`deep_search` partial-artifact contract.
        let temp = tempfile::tempdir().unwrap();
        // Drop a primary report so the hard-required validator passes; the
        // soft-fail one points at a non-existent sub-artifact and warns.
        let primary = temp.path().join("primary.md");
        std::fs::write(&primary, b"primary report").unwrap();

        let mut policy = WorkspacePolicy::for_session();
        policy.spawn_tasks.insert(
            "partial_artifact_task".into(),
            WorkspaceSpawnTaskPolicy {
                artifact: None,
                artifacts: Vec::new(),
                on_verify: Vec::new(),
                on_complete: Vec::new(),
                on_deliver: Vec::new(),
                on_failure: vec!["notify_user:partial failed".into()],
                on_completion: vec![
                    // Hard-required: must pass for the spawn task to satisfy.
                    crate::workspace_policy::SpawnTaskValidatorSpec::Full(Validator {
                        id: "primary_required".into(),
                        required: true,
                        soft_fail: false,
                        timeout_ms: None,
                        phase: ValidatorPhaseKind::Completion,
                        spec: crate::workspace_policy::ValidatorSpec::FileExists {
                            path: "primary.md".into(),
                            min_bytes: None,
                        },
                    }),
                    // Soft-fail: surfaces as a warning without demoting the
                    // gate even though `required = true`.
                    crate::workspace_policy::SpawnTaskValidatorSpec::Full(Validator {
                        id: "sub_artifact_warn".into(),
                        required: true,
                        soft_fail: true,
                        timeout_ms: None,
                        phase: ValidatorPhaseKind::Completion,
                        spec: crate::workspace_policy::ValidatorSpec::FileExists {
                            path: "sub-artifact.md".into(),
                            min_bytes: None,
                        },
                    }),
                ],
            },
        );
        write_workspace_policy(temp.path(), &policy).unwrap();

        let result = enforce_spawn_task_contract_with_args(
            &ToolRegistry::with_builtins(temp.path()),
            "partial_artifact_task",
            "tool-call-soft-fail",
            &[],
            UNIX_EPOCH,
            None,
            None,
        )
        .await;

        // Soft-fail validator must NOT demote the task even though it
        // failed. The ledger still records the failure for operator
        // visibility (covered by validators::tests::soft_fail_*).
        match result {
            SpawnTaskContractResult::Satisfied { .. } => {}
            other => panic!("expected satisfied (soft-fail must not block), got {other:?}"),
        }

        let ledger_path = temp.path().join(".octos").join("validator_outcomes.jsonl");
        let ledger = crate::validators::ValidatorLedger::open(&ledger_path).unwrap();
        let outcomes = ledger.read_all().unwrap();
        let warn = outcomes
            .iter()
            .find(|o| o.validator_id == "sub_artifact_warn")
            .expect("soft-fail warning should persist to the ledger");
        assert_eq!(warn.required_tier, "soft");
        assert!(
            !warn.required,
            "soft-fail must surface as required = false to legacy replayers"
        );
        assert!(warn.is_soft_warning());
    }

    /// Helper used by the `podcast_generate` per-file-non-silent e2e
    /// tests. PCM WAV with a sine wave loud enough to clear the
    /// validator's non-silent-sample floor.
    fn write_sine_wav_at(path: &std::path::Path, samples: usize) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 8_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create_dir_all");
        let mut writer = hound::WavWriter::create(path, spec).expect("create wav");
        let amplitude = i16::MAX / 2;
        for index in 0..samples {
            let phase = (index as f32) * std::f32::consts::TAU * 440.0 / 8000.0;
            let value = (phase.sin() * amplitude as f32) as i16;
            // Keep value away from zero crossings to ensure non-silent floor.
            let value = if value.abs() < 4_000 { 4_000 } else { value };
            writer.write_sample(value).expect("write sample");
        }
        writer.finalize().expect("finalize wav");
    }

    /// Companion to [`write_sine_wav_at`]: a WAV filled with PCM zeros so
    /// the validator's non-silent ratio drops to 0.0. Used to drive the
    /// per-file failure path.
    fn write_silent_wav_at(path: &std::path::Path, samples: usize) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 8_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create_dir_all");
        let mut writer = hound::WavWriter::create(path, spec).expect("create wav");
        for _ in 0..samples {
            writer.write_sample(0i16).expect("write sample");
        }
        writer.finalize().expect("finalize wav");
    }

    /// Rewire the `podcast_generate` contract's MP3-targeted whole-file
    /// validators to WAV equivalents so we can drive the gate with real
    /// `hound` output (without requiring the `audio_mp3` feature). Keeps
    /// the `PerFileNonSilent` validator unchanged so the segment-level
    /// gate runs as configured.
    fn retarget_podcast_contract_to_wav(policy: &mut crate::WorkspacePolicy) {
        use crate::workspace_policy::{MagicByteKind, SpawnTaskValidatorSpec, ValidatorSpec};
        let task = policy
            .spawn_tasks
            .get_mut("podcast_generate")
            .expect("podcast_generate must exist in for_session policy");
        for entry in task.on_completion.iter_mut() {
            if let SpawnTaskValidatorSpec::Bare(spec) = entry {
                match spec {
                    ValidatorSpec::AudioNonSilent { glob, .. } => {
                        *glob = "skill-output/mofa-podcast/*.wav".into();
                    }
                    ValidatorSpec::MagicBytes { glob, format } => {
                        *glob = "skill-output/mofa-podcast/*.wav".into();
                        *format = MagicByteKind::Wav;
                    }
                    _ => {}
                }
            }
        }
    }

    /// Wave-3b end-to-end: when every preserved segment WAV is non-silent
    /// AND the assembled artifact is non-silent, the combined contract
    /// gate (MagicBytes + AudioNonSilent whole-file + PerFileNonSilent
    /// per-segment) must satisfy.
    #[tokio::test]
    async fn podcast_contract_satisfies_when_all_segments_and_final_mix_are_non_silent() {
        let temp = tempfile::tempdir().unwrap();
        let mut policy = crate::WorkspacePolicy::for_session();
        retarget_podcast_contract_to_wav(&mut policy);
        write_workspace_policy(temp.path(), &policy).unwrap();

        // The assembled (whole-file) artifact. Drives MagicBytes + the
        // whole-file AudioNonSilent.
        let assembled = temp.path().join("skill-output/mofa-podcast/podcast.wav");
        // 16-bit mono PCM at 8 kHz needs ~2000+ samples to clear the
        // `file_size_min:$artifact:4096` policy gate.
        write_sine_wav_at(&assembled, 4_000);

        // The preserved per-segment WAVs that mofa-skills #59 leaves on
        // disk after successful assembly. The PerFileNonSilent glob
        // (`**/segments/seg_*.wav`) scopes to JUST these — placeholder
        // pause/BGM files share the directory but are excluded.
        let seg_dir = temp
            .path()
            .join("skill-output/mofa-podcast/episode_001/segments");
        write_sine_wav_at(&seg_dir.join("seg_000_alice.wav"), 800);
        write_sine_wav_at(&seg_dir.join("seg_001_bob.wav"), 800);
        write_silent_wav_at(&seg_dir.join("pause_after_000.wav"), 400);
        write_silent_wav_at(&seg_dir.join("pause_line_001.wav"), 400);
        write_silent_wav_at(&seg_dir.join("bgm_placeholder_line_001.wav"), 400);

        let result = enforce_spawn_task_contract_with_args(
            &ToolRegistry::with_builtins(temp.path()),
            "podcast_generate",
            "tool-call-podcast-happy",
            std::slice::from_ref(&assembled),
            UNIX_EPOCH,
            None,
            None,
        )
        .await;

        match result {
            SpawnTaskContractResult::Satisfied { .. } => {}
            other => panic!("expected satisfied, got {other:?}"),
        }

        // Both gates must have fired and persisted to the ledger so
        // operators can confirm the combined contract ran. We assert on
        // the typed `kind` discriminator so a future refactor of the
        // reason string doesn't break this contract.
        let ledger_path = temp.path().join(".octos").join("validator_outcomes.jsonl");
        let ledger = crate::validators::ValidatorLedger::open(&ledger_path).unwrap();
        let outcomes = ledger.read_all().unwrap();
        assert!(
            outcomes.iter().any(|o| o.kind == "audio_non_silent"),
            "whole-file AudioNonSilent must have fired: {outcomes:?}"
        );
        assert!(
            outcomes.iter().any(|o| o.kind == "per_file_non_silent"),
            "per-segment PerFileNonSilent must have fired: {outcomes:?}"
        );
    }

    /// Adversarial e2e: one segment WAV is silent. The whole-file
    /// AudioNonSilent still passes (the silent gap is averaged out by
    /// the surrounding loud assembled audio), but PerFileNonSilent
    /// rejects the spawn task at the gate and surfaces the offending
    /// segment basename in the typed failure error. This is the bug
    /// class the variant was introduced to catch.
    #[tokio::test]
    async fn podcast_contract_fails_when_a_single_segment_is_silent_even_if_final_mix_is_loud() {
        let temp = tempfile::tempdir().unwrap();
        let mut policy = crate::WorkspacePolicy::for_session();
        retarget_podcast_contract_to_wav(&mut policy);
        write_workspace_policy(temp.path(), &policy).unwrap();

        // Final mix is fully loud — whole-file AudioNonSilent would
        // accept it on its own.
        let assembled = temp.path().join("skill-output/mofa-podcast/podcast.wav");
        // 16-bit mono PCM at 8 kHz needs ~2000+ samples to clear the
        // `file_size_min:$artifact:4096` policy gate.
        write_sine_wav_at(&assembled, 4_000);

        let seg_dir = temp
            .path()
            .join("skill-output/mofa-podcast/episode_002/segments");
        write_sine_wav_at(&seg_dir.join("seg_000_alice.wav"), 800);
        // The bad apple. PerFileNonSilent must catch this even though
        // the whole-file validator above does not.
        write_silent_wav_at(&seg_dir.join("seg_001_bob.wav"), 800);
        write_sine_wav_at(&seg_dir.join("seg_002_alice.wav"), 800);

        let result = enforce_spawn_task_contract_with_args(
            &ToolRegistry::with_builtins(temp.path()),
            "podcast_generate",
            "tool-call-podcast-silent-seg",
            std::slice::from_ref(&assembled),
            UNIX_EPOCH,
            None,
            None,
        )
        .await;

        match result {
            SpawnTaskContractResult::Failed { error, notify_user } => {
                assert!(
                    error.contains("per_file_non_silent") && error.contains("seg_001_bob.wav"),
                    "failure must surface the offending segment filename: {error}"
                );
                assert_eq!(notify_user.as_deref(), Some("Podcast generation failed"));
            }
            other => panic!("expected failure, got {other:?}"),
        }
    }
}
