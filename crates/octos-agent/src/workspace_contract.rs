use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use glob::glob;

use crate::behaviour::{
    ActionContext, ActionResult, evaluate_actions_with_context, run_action_with_context,
};
use crate::sandbox::Sandbox;
use crate::task_supervisor::{TaskRuntimeState, TaskSupervisor};
use crate::tools::{ToolContext, ToolRegistry};
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
    sandbox: Arc<dyn Sandbox>,
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
        sandbox,
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
#[allow(clippy::too_many_arguments)]
pub async fn enforce_spawn_task_contract_with_args(
    tools: &ToolRegistry,
    tool_name: &str,
    tool_call_id: &str,
    files_to_send: &[PathBuf],
    task_started_at: SystemTime,
    supervisor: Option<(&TaskSupervisor, &str)>,
    input_args: Option<&serde_json::Value>,
    sandbox: Arc<dyn Sandbox>,
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
        sandbox,
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
    sandbox: Arc<dyn Sandbox>,
) -> SpawnTaskContractResult {
    enforce_spawn_task_contract_with_args_and_output_and_context(
        tools,
        tool_name,
        tool_call_id,
        files_to_send,
        task_started_at,
        supervisor,
        input_args,
        tool_named_outputs,
        sandbox,
        &ToolContext::zero(),
    )
    .await
}

/// Context-bearing contract variant: delivery tools receive the same turn scope
/// as the originating foreground/background tool instead of a zero context.
#[allow(clippy::too_many_arguments)]
pub async fn enforce_spawn_task_contract_with_args_and_output_and_context(
    tools: &ToolRegistry,
    tool_name: &str,
    tool_call_id: &str,
    files_to_send: &[PathBuf],
    task_started_at: SystemTime,
    supervisor: Option<(&TaskSupervisor, &str)>,
    input_args: Option<&serde_json::Value>,
    tool_named_outputs: Option<&serde_json::Value>,
    sandbox: Arc<dyn Sandbox>,
    tool_context: &ToolContext,
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
        // octos #1034: forward the plugin's `files_to_send` so the
        // file-list-driven validators (`MagicBytes`, `AudioNonSilent`,
        // `PerFileNonSilent`) declaring `source = "spawn_only_files"`
        // can consume the authoritative path set the skill emitted.
        Some(files_to_send.to_vec()),
        sandbox,
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
        tool_context,
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
    tool_context: &ToolContext,
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
                match tools
                    .execute_with_context(tool_context, "send_file", &send_args)
                    .await
                {
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
///
/// #1607: `sandbox` is the session sandbox under which `ValidatorSpec::Command`
/// validators (which a project's `workspace_policy.toml` can declare) execute.
/// It is threaded EXPLICITLY — the compiler enforces that every call site names
/// the sandbox it means to confine command validators to, so an untrusted
/// workspace validator can never run unsandboxed on the host from a sandboxed
/// session. Pass `Arc::new(crate::sandbox::NoSandbox)` for host-independent
/// contexts (a no-op sandbox runs the argv directly, matching pre-#1607
/// behaviour).
pub async fn run_declared_validators(
    tools: &ToolRegistry,
    workspace_root: &Path,
    validators: &[Validator],
    repo_label_hint: &str,
    phase: ValidatorPhase,
    input_args: Option<serde_json::Value>,
    sandbox: Arc<dyn Sandbox>,
) -> Result<Vec<ValidatorOutcome>, String> {
    run_declared_validators_with_output(
        tools,
        workspace_root,
        validators,
        repo_label_hint,
        phase,
        input_args,
        None,
        None,
        sandbox,
    )
    .await
}

/// Variant of [`run_declared_validators`] that also threads the spawn
/// task's `named_outputs` (`tool_output`) into the validator invocation so
/// domain validators can resolve `${output.<key>}` references against
/// tool-emitted values (e.g. `mofa_publish` emitting `deploy_url` for the
/// HttpProbe to call).
///
/// `spawn_only_files` is the plugin-reported `files_to_send` list from the
/// originating spawn_only tool. Consumed by file-list-driven validators
/// (`MagicBytes`, `AudioNonSilent`, `PerFileNonSilent`) when their spec
/// declares `source = "spawn_only_files"` (octos #1034). Pass `None` for
/// callers that have no plugin output to forward (turn-end validators,
/// non-spawn contexts).
#[allow(clippy::too_many_arguments)]
pub async fn run_declared_validators_with_output(
    tools: &ToolRegistry,
    workspace_root: &Path,
    validators: &[Validator],
    repo_label_hint: &str,
    phase: ValidatorPhase,
    input_args: Option<serde_json::Value>,
    tool_output: Option<serde_json::Value>,
    spawn_only_files: Option<Vec<PathBuf>>,
    sandbox: Arc<dyn Sandbox>,
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

    let runner = build_validator_runner(tools, workspace_root, sandbox);
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
        spawn_only_files: spawn_only_files.unwrap_or_default(),
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
    files_to_send: &[PathBuf],
    sandbox: Arc<dyn Sandbox>,
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

        // Filter `files_to_send` to the files that belong to THIS
        // project. The host rebinds plugin work_dir to
        // `<session>/skill-output/`, so a slides project's deck lands
        // at `<session>/skill-output/slides/<slug>/output/deck.pptx`.
        // Tests / legacy paths may also stage files directly under
        // `<session>/<kind>/<slug>/`. The `SpawnOnlyFiles` validator
        // source consumes this filtered list.
        let project_files =
            filter_files_for_project(files_to_send, working_dir, repo.kind, &repo.slug);

        report.projects_run = report.projects_run.saturating_add(1);
        match run_declared_validators_with_output(
            tools,
            &project_root,
            &policy.validation.validators,
            &repo_label,
            ValidatorPhase::Completion,
            None,
            None,
            Some(project_files),
            sandbox.clone(),
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

/// Select files from `files_to_send` whose absolute path lives under
/// either `<session>/skill-output/<kind>/<slug>/` (the canonical
/// post-rebind plugin output location) or `<session>/<kind>/<slug>/`
/// (legacy / test fallback where files were staged directly inside the
/// project dir).
fn filter_files_for_project(
    files_to_send: &[PathBuf],
    session_root: &Path,
    kind: WorkspaceProjectKind,
    slug: &str,
) -> Vec<PathBuf> {
    let kind_dir = kind.directory_name();
    let prefix_skill_output = session_root.join("skill-output").join(kind_dir).join(slug);
    let prefix_in_project = session_root.join(kind_dir).join(slug);
    files_to_send
        .iter()
        .filter(|path| {
            let absolute = if path.is_absolute() {
                (*path).clone()
            } else {
                session_root.join(path)
            };
            absolute.starts_with(&prefix_skill_output) || absolute.starts_with(&prefix_in_project)
        })
        .cloned()
        .collect()
}

fn build_validator_runner(
    tools: &ToolRegistry,
    workspace_root: &Path,
    sandbox: Arc<dyn Sandbox>,
) -> ValidatorRunner {
    // Capture a lightweight snapshot of tool handles for the validator runner.
    // Avoids cloning the full registry and its LRU bookkeeping.
    let dispatcher: Arc<dyn crate::validators::ValidatorToolDispatcher> =
        Arc::new(crate::validators::MapToolDispatcher::from_registry(tools));
    // #1607: confine `ValidatorSpec::Command` validators (which a project's
    // `workspace_policy.toml` can declare) to the session sandbox the caller
    // hands in EXPLICITLY, so the automatic project-root validator pass at the
    // end of `run_task` can't be turned into a host-level sandbox escape. The
    // sandbox is an explicit entry-point argument (not `tools.sandbox()`) so
    // the compiler enumerates and enforces every call site: each caller must
    // name the sandbox it means to confine command validators to.
    // `ValidatorRunner::run_command` runs the argv directly when the sandbox is
    // a no-op (`NoSandbox`, or a backend whose helper is unavailable), so hosts
    // without a real backend are unaffected; on POSIX with a real backend the
    // command is shell-quoted and wrapped; the Windows real-sandbox case fails
    // closed.
    ValidatorRunner::with_dispatcher(dispatcher, workspace_root).with_sandbox(sandbox)
}
