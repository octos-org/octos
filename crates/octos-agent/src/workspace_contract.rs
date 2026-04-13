use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use glob::glob;

use crate::task_supervisor::{TaskRuntimeState, TaskSupervisor};
use crate::tools::ToolRegistry;
use crate::workspace_policy::{WorkspacePolicy, WorkspaceSpawnTaskPolicy, read_workspace_policy};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnTaskContractResult {
    NotConfigured,
    Satisfied {
        delivered_files: Vec<String>,
    },
    Failed {
        error: String,
        notify_user: Option<String>,
    },
}

pub fn requires_workspace_contract(tool_name: &str) -> bool {
    matches!(tool_name, "fm_tts" | "voice_synthesize")
}

pub async fn enforce_spawn_task_contract(
    tools: &ToolRegistry,
    tool_name: &str,
    tool_call_id: &str,
    files_to_send: &[PathBuf],
    task_started_at: SystemTime,
    supervisor: Option<(&TaskSupervisor, &str)>,
) -> SpawnTaskContractResult {
    let Some(workspace_root) = tools.workspace_root() else {
        return SpawnTaskContractResult::NotConfigured;
    };

    let policy = match read_workspace_policy(workspace_root) {
        Ok(Some(policy)) => policy,
        Ok(None) => return SpawnTaskContractResult::NotConfigured,
        Err(error) => {
            return SpawnTaskContractResult::Failed {
                error: format!("workspace contract read failed: {error}"),
                notify_user: None,
            };
        }
    };

    let Some(task_policy) = policy.spawn_tasks.get(tool_name) else {
        return SpawnTaskContractResult::NotConfigured;
    };

    let notify_user = extract_notify_user(task_policy);

    set_runtime_state(
        supervisor,
        TaskRuntimeState::ResolvingOutputs,
        Some(format!("resolve outputs for {tool_name}")),
    );
    let artifact_paths = match resolve_artifacts(
        workspace_root,
        &policy,
        task_policy,
        files_to_send,
        task_started_at,
    ) {
        Ok(paths) => paths,
        Err(error) => {
            run_failure_actions(workspace_root, supervisor, &task_policy.on_failure);
            return SpawnTaskContractResult::Failed { error, notify_user };
        }
    };

    set_runtime_state(
        supervisor,
        TaskRuntimeState::VerifyingOutputs,
        Some(format!("verify outputs for {tool_name}")),
    );
    if let Err(error) = run_verify_actions(workspace_root, &task_policy.on_verify, &artifact_paths)
    {
        run_failure_actions(workspace_root, supervisor, &task_policy.on_failure);
        return SpawnTaskContractResult::Failed { error, notify_user };
    }

    set_runtime_state(
        supervisor,
        TaskRuntimeState::DeliveringOutputs,
        Some(format!("deliver outputs for {tool_name}")),
    );
    match run_complete_actions(
        tools,
        workspace_root,
        tool_call_id,
        &task_policy.on_complete,
        &artifact_paths,
    )
    .await
    {
        Ok(delivered_files) => SpawnTaskContractResult::Satisfied { delivered_files },
        Err(error) => {
            run_failure_actions(workspace_root, supervisor, &task_policy.on_failure);
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
) -> Result<Vec<PathBuf>, String> {
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
        return Ok(files);
    }

    let artifact_name = task_policy
        .artifact
        .as_deref()
        .ok_or_else(|| "workspace contract has no artifact source".to_string())?;
    let pattern = policy.artifacts.entries.get(artifact_name).ok_or_else(|| {
        format!("workspace contract references unknown artifact '{artifact_name}'")
    })?;

    let mut matches = resolve_glob_matches(workspace_root, pattern, Some(task_started_at))?;
    if matches.is_empty() {
        matches = resolve_glob_matches(workspace_root, pattern, None)?;
    }
    if matches.is_empty() {
        return Err(format!(
            "contract could not find artifact matching '{pattern}'"
        ));
    }
    Ok(matches)
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
    artifact_paths: &[PathBuf],
) -> Result<(), String> {
    for action in actions {
        if let Some(target) = action.strip_prefix("file_exists:") {
            let targets = resolve_action_targets(workspace_root, target, artifact_paths)?;
            if targets.is_empty() || targets.iter().any(|path| !path.exists()) {
                return Err(format!("verify failed: expected file for '{target}'"));
            }
            continue;
        }

        if let Some(rest) = action.strip_prefix("file_size_min:") {
            let mut parts = rest.splitn(2, ':');
            let target = parts
                .next()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| format!("invalid verify action '{action}'"))?;
            let min_size = parts
                .next()
                .ok_or_else(|| format!("invalid verify action '{action}'"))?
                .parse::<u64>()
                .map_err(|error| format!("invalid minimum size in '{action}': {error}"))?;
            for path in resolve_action_targets(workspace_root, target, artifact_paths)? {
                let size = std::fs::metadata(&path)
                    .map_err(|error| format!("stat failed for {}: {error}", path.display()))?
                    .len();
                if size < min_size {
                    return Err(format!(
                        "verify failed: {} is {} bytes (< {})",
                        path.display(),
                        size,
                        min_size
                    ));
                }
            }
            continue;
        }

        return Err(format!("unsupported verify action '{action}'"));
    }

    Ok(())
}

async fn run_complete_actions(
    tools: &ToolRegistry,
    workspace_root: &Path,
    tool_call_id: &str,
    actions: &[String],
    artifact_paths: &[PathBuf],
) -> Result<Vec<String>, String> {
    let mut delivered_files = Vec::new();

    for action in actions {
        if let Some(target) = action.strip_prefix("send_file:") {
            for path in resolve_action_targets(workspace_root, target, artifact_paths)? {
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

        if let Some(pattern) = action.strip_prefix("cleanup:") {
            run_cleanup_action(workspace_root, pattern)?;
            continue;
        }

        return Err(format!("unsupported completion action '{action}'"));
    }

    Ok(delivered_files)
}

fn run_failure_actions(
    workspace_root: &Path,
    supervisor: Option<(&TaskSupervisor, &str)>,
    actions: &[String],
) {
    if actions.iter().any(|action| action.starts_with("cleanup:")) {
        set_runtime_state(
            supervisor,
            TaskRuntimeState::CleaningUp,
            Some("cleanup failed outputs".to_string()),
        );
    }
    for action in actions {
        if let Some(pattern) = action.strip_prefix("cleanup:") {
            let _ = run_cleanup_action(workspace_root, pattern);
        }
    }
}

fn run_cleanup_action(workspace_root: &Path, pattern: &str) -> Result<(), String> {
    for path in resolve_glob_matches(workspace_root, pattern, None)? {
        if path.is_dir() {
            std::fs::remove_dir_all(&path)
                .map_err(|error| format!("cleanup failed for {}: {error}", path.display()))?;
        } else if path.exists() {
            std::fs::remove_file(&path)
                .map_err(|error| format!("cleanup failed for {}: {error}", path.display()))?;
        }
    }
    Ok(())
}

fn resolve_action_targets(
    workspace_root: &Path,
    target: &str,
    artifact_paths: &[PathBuf],
) -> Result<Vec<PathBuf>, String> {
    if target == "$artifact" {
        return Ok(artifact_paths.to_vec());
    }
    resolve_glob_matches(workspace_root, target, None)
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

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use super::*;
    use crate::{SendFileTool, ToolRegistry, WorkspacePolicy, write_workspace_policy};

    #[tokio::test]
    async fn tts_contract_auto_sends_new_mp3() {
        let temp = tempfile::tempdir().unwrap();
        write_workspace_policy(temp.path(), &WorkspacePolicy::for_session()).unwrap();
        let output = temp.path().join("tts_result.mp3");
        std::fs::write(&output, vec![1u8; 2048]).unwrap();

        let mut tools = ToolRegistry::with_builtins(temp.path());
        let (tx, mut rx) = mpsc::channel(4);
        tools.register(SendFileTool::with_context(tx, "api", "sess-1").with_base_dir(temp.path()));

        let result =
            enforce_spawn_task_contract(&tools, "fm_tts", "tool-call-1", &[], UNIX_EPOCH, None)
                .await;

        match result {
            SpawnTaskContractResult::Satisfied { delivered_files } => {
                assert_eq!(delivered_files, vec![output.to_string_lossy().to_string()]);
            }
            other => panic!("expected success, got {other:?}"),
        }

        let msg = rx.recv().await.expect("file message");
        assert_eq!(msg.media, vec![output.to_string_lossy().to_string()]);
        assert_eq!(
            msg.metadata
                .get("tool_call_id")
                .and_then(|value| value.as_str()),
            Some("tool-call-1")
        );
    }

    #[tokio::test]
    async fn tts_contract_fails_when_no_mp3_exists() {
        let temp = tempfile::tempdir().unwrap();
        write_workspace_policy(temp.path(), &WorkspacePolicy::for_session()).unwrap();

        let mut tools = ToolRegistry::with_builtins(temp.path());
        let (tx, _rx) = mpsc::channel(4);
        tools.register(SendFileTool::with_context(tx, "api", "sess-1").with_base_dir(temp.path()));

        let result =
            enforce_spawn_task_contract(&tools, "fm_tts", "tool-call-2", &[], UNIX_EPOCH, None)
                .await;

        match result {
            SpawnTaskContractResult::Failed { error, notify_user } => {
                assert!(error.contains("artifact"));
                assert_eq!(notify_user.as_deref(), Some("TTS generation failed"));
            }
            other => panic!("expected failure, got {other:?}"),
        }
    }
}
