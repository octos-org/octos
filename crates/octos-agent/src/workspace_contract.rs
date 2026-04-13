//! Workspace contract enforcement for spawn_only background tasks.
//!
//! A contract defines what must be true before a background task is considered
//! "completed": output files exist, pass size checks, and get delivered.
//! The contract runs inline between tool execution and supervisor status update,
//! so `mark_completed` only fires after verification passes.
//!
//! This is NOT a post-processing step — it gates the status transition itself.

use std::path::Path;

use tracing::{info, warn};

use crate::behaviour;
use crate::workspace_policy::{SpawnTaskPolicy, read_workspace_policy};

/// Result of enforcing a spawn task contract.
#[derive(Debug)]
pub enum ContractVerdict {
    /// All verify actions passed. `on_complete` actions have been run.
    /// Contains notification messages from any `notify_user` actions.
    Satisfied { notifications: Vec<String> },
    /// Verify failed or on_failure actions were run.
    /// Contains the failure reasons and any notification messages.
    Failed {
        reasons: Vec<String>,
        notifications: Vec<String>,
    },
    /// No contract defined for this tool — pass through to existing behaviour.
    NoContract,
}

/// Look up and enforce the workspace contract for a spawn_only tool.
///
/// Call this AFTER the tool executes successfully but BEFORE `mark_completed`.
/// If this returns `Failed`, call `mark_failed` instead.
pub fn enforce(workspace_root: &Path, tool_name: &str) -> ContractVerdict {
    let policy = match read_workspace_policy(workspace_root) {
        Ok(Some(p)) => p,
        Ok(None) => return ContractVerdict::NoContract,
        Err(e) => {
            warn!(tool = %tool_name, error = %e, "failed to read workspace policy for contract");
            return ContractVerdict::NoContract;
        }
    };

    let Some(task_policy) = policy.spawn_tasks.get(tool_name) else {
        return ContractVerdict::NoContract;
    };

    if task_policy.on_verify.is_empty() && task_policy.on_complete.is_empty() {
        return ContractVerdict::NoContract;
    };

    enforce_policy(workspace_root, tool_name, task_policy)
}

/// Enforce a specific spawn task policy. Separated for testability.
pub fn enforce_policy(
    workspace_root: &Path,
    tool_name: &str,
    policy: &SpawnTaskPolicy,
) -> ContractVerdict {
    // Step 1: Run verify actions
    if !policy.on_verify.is_empty() {
        match behaviour::run_actions(workspace_root, &policy.on_verify) {
            Ok(results) => {
                if !behaviour::all_passed(&results) {
                    let reasons = behaviour::failure_reasons(&results);
                    warn!(
                        tool = %tool_name,
                        failures = ?reasons,
                        "spawn task contract verify failed"
                    );
                    // Run on_failure actions
                    let notifications = run_failure_actions(workspace_root, tool_name, policy);
                    return ContractVerdict::Failed {
                        reasons,
                        notifications,
                    };
                }
            }
            Err(e) => {
                warn!(tool = %tool_name, error = %e, "contract verify actions errored");
                let notifications = run_failure_actions(workspace_root, tool_name, policy);
                return ContractVerdict::Failed {
                    reasons: vec![format!("verify error: {e}")],
                    notifications,
                };
            }
        }
    }

    // Step 2: Run on_complete actions
    let mut notifications = Vec::new();
    if !policy.on_complete.is_empty() {
        match behaviour::run_actions(workspace_root, &policy.on_complete) {
            Ok(results) => {
                notifications = behaviour::notifications(&results);
                if !behaviour::all_passed(&results) {
                    let reasons = behaviour::failure_reasons(&results);
                    warn!(
                        tool = %tool_name,
                        failures = ?reasons,
                        "spawn task contract on_complete actions failed"
                    );
                    return ContractVerdict::Failed {
                        reasons,
                        notifications,
                    };
                }
            }
            Err(e) => {
                warn!(tool = %tool_name, error = %e, "on_complete actions errored");
                return ContractVerdict::Failed {
                    reasons: vec![format!("on_complete error: {e}")],
                    notifications,
                };
            }
        }
    }

    info!(
        tool = %tool_name,
        notifications = notifications.len(),
        "spawn task contract satisfied"
    );
    ContractVerdict::Satisfied { notifications }
}

/// Run on_failure actions and collect notifications.
fn run_failure_actions(
    workspace_root: &Path,
    tool_name: &str,
    policy: &SpawnTaskPolicy,
) -> Vec<String> {
    if policy.on_failure.is_empty() {
        return Vec::new();
    }
    match behaviour::run_actions(workspace_root, &policy.on_failure) {
        Ok(results) => behaviour::notifications(&results),
        Err(e) => {
            warn!(tool = %tool_name, error = %e, "on_failure actions errored");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_git::WorkspaceProjectKind;
    use crate::workspace_policy::{WorkspacePolicy, write_workspace_policy};

    #[test]
    fn should_return_no_contract_when_no_policy() {
        let temp = tempfile::tempdir().unwrap();
        let verdict = enforce(temp.path(), "tts");
        assert!(matches!(verdict, ContractVerdict::NoContract));
    }

    #[test]
    fn should_return_no_contract_when_tool_not_in_policy() {
        let temp = tempfile::tempdir().unwrap();
        let policy = WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides);
        write_workspace_policy(temp.path(), &policy).unwrap();

        let verdict = enforce(temp.path(), "unknown_tool");
        assert!(matches!(verdict, ContractVerdict::NoContract));
    }

    #[test]
    fn should_satisfy_when_verify_passes() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("output.mp3"), b"audio data here").unwrap();

        let mut policy = WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides);
        policy.spawn_tasks.insert(
            "tts".into(),
            SpawnTaskPolicy {
                on_verify: vec!["file_exists:output.mp3".into()],
                on_complete: vec!["notify_user:Audio ready".into()],
                on_failure: vec![],
            },
        );
        write_workspace_policy(temp.path(), &policy).unwrap();

        let verdict = enforce(temp.path(), "tts");
        match verdict {
            ContractVerdict::Satisfied { notifications } => {
                assert_eq!(notifications, vec!["Audio ready"]);
            }
            other => panic!("expected Satisfied, got {other:?}"),
        }
    }

    #[test]
    fn should_fail_when_verify_fails() {
        let temp = tempfile::tempdir().unwrap();
        // Do NOT create output.mp3

        let mut policy = WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides);
        policy.spawn_tasks.insert(
            "tts".into(),
            SpawnTaskPolicy {
                on_verify: vec!["file_exists:output.mp3".into()],
                on_complete: vec!["notify_user:Audio ready".into()],
                on_failure: vec!["notify_user:TTS failed".into()],
            },
        );
        write_workspace_policy(temp.path(), &policy).unwrap();

        let verdict = enforce(temp.path(), "tts");
        match verdict {
            ContractVerdict::Failed {
                reasons,
                notifications,
            } => {
                assert!(!reasons.is_empty());
                assert!(reasons[0].contains("no files match"));
                assert_eq!(notifications, vec!["TTS failed"]);
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn should_run_cleanup_on_failure() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("temp")).unwrap();
        std::fs::write(temp.path().join("temp/tts_work.wav"), b"temp data").unwrap();

        let mut policy = WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides);
        policy.spawn_tasks.insert(
            "tts".into(),
            SpawnTaskPolicy {
                on_verify: vec!["file_exists:output.mp3".into()],
                on_complete: vec![],
                on_failure: vec!["cleanup:temp/tts_*".into()],
            },
        );
        write_workspace_policy(temp.path(), &policy).unwrap();

        let verdict = enforce(temp.path(), "tts");
        assert!(matches!(verdict, ContractVerdict::Failed { .. }));
        assert!(!temp.path().join("temp/tts_work.wav").exists());
    }

    #[test]
    fn should_fail_when_file_too_small() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("output.mp3"), b"x").unwrap(); // 1 byte

        let mut policy = WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides);
        policy.spawn_tasks.insert(
            "tts".into(),
            SpawnTaskPolicy {
                on_verify: vec!["file_size_min:output.mp3:1024".into()],
                on_complete: vec![],
                on_failure: vec![],
            },
        );
        write_workspace_policy(temp.path(), &policy).unwrap();

        let verdict = enforce(temp.path(), "tts");
        match verdict {
            ContractVerdict::Failed { reasons, .. } => {
                assert!(reasons[0].contains("bytes"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }
}
