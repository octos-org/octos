//! Deterministic behaviour actions for workspace policy enforcement.
//!
//! Actions are simple string specs like `"file_exists:output/*.mp3"` parsed into
//! an action kind + argument. They run without LLM involvement and are used for
//! workspace inspection, spawn_only task verification, turn-end validation, and
//! cleanup.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use eyre::{Result, eyre};
use glob::glob;
use tracing::{info, warn};

/// Result of running a single behaviour action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionResult {
    Pass,
    Fail {
        reason: String,
    },
    /// Action succeeded and requests a user notification with the given message.
    /// Callers should deliver this through the appropriate channel (SSE, Telegram, etc.).
    Notify {
        message: String,
    },
}

impl ActionResult {
    pub fn is_pass(&self) -> bool {
        matches!(self, Self::Pass | Self::Notify { .. })
    }
}

/// Optional named target resolution for actions that need runtime-bound paths.
///
/// This is used by spawn-task contracts so shared actions such as
/// `file_exists:$artifact` and `file_size_min:$artifact:1024` resolve against
/// the verified output candidates rather than a static glob.
#[derive(Debug, Clone, Default)]
pub(crate) struct ActionContext {
    named_targets: BTreeMap<String, Vec<PathBuf>>,
}

impl ActionContext {
    pub(crate) fn with_named_target(
        mut self,
        name: impl Into<String>,
        targets: Vec<PathBuf>,
    ) -> Self {
        self.named_targets.insert(name.into(), targets);
        self
    }

    pub(crate) fn with_named_targets<I, N>(mut self, targets: I) -> Self
    where
        I: IntoIterator<Item = (N, Vec<PathBuf>)>,
        N: Into<String>,
    {
        for (name, paths) in targets {
            self.named_targets.insert(name.into(), paths);
        }
        self
    }

    pub(crate) fn resolve_targets(
        &self,
        workspace_root: &Path,
        target: &str,
    ) -> Result<Vec<PathBuf>> {
        if target.starts_with('$') {
            return Ok(self.named_targets.get(target).cloned().unwrap_or_default());
        }
        resolve_glob(workspace_root, target)
    }
}

/// Extract notification messages from action results.
pub fn notifications(results: &[(String, ActionResult)]) -> Vec<String> {
    results
        .iter()
        .filter_map(|(_, r)| match r {
            ActionResult::Notify { message } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

/// Parse and execute a behaviour action spec against a workspace root.
///
/// Action specs follow the format `"action_kind:argument"`.
///
/// Supported actions:
/// - `file_exists:<glob>` — at least one file matches the glob pattern
/// - `file_size_min:<glob>:<bytes>` — matched files are at least N bytes
/// - `cleanup:<glob>` — remove files matching the glob (always passes)
/// - `notify_user:<message>` — log a notification (always passes, actual
///   delivery wired by caller)
pub fn run_action(workspace_root: &Path, spec: &str) -> Result<ActionResult> {
    run_action_with_context(workspace_root, &ActionContext::default(), spec)
}

pub(crate) fn run_action_with_context(
    workspace_root: &Path,
    context: &ActionContext,
    spec: &str,
) -> Result<ActionResult> {
    let (kind, arg) = parse_spec(spec)?;

    match kind {
        "file_exists" => action_file_exists(workspace_root, context, arg),
        "file_size_min" => action_file_size_min(workspace_root, context, arg),
        "cleanup" => action_cleanup(workspace_root, context, arg),
        "notify_user" => action_notify_user(arg),
        _ => Err(eyre!("unknown behaviour action: {kind}")),
    }
}

/// Run a list of action specs. Returns all results. Stops early on error
/// (action parse/execution failure), but NOT on `ActionResult::Fail`.
pub fn run_actions(workspace_root: &Path, specs: &[String]) -> Result<Vec<(String, ActionResult)>> {
    run_actions_with_context(workspace_root, &ActionContext::default(), specs)
}

pub(crate) fn evaluate_actions_with_context(
    workspace_root: &Path,
    context: &ActionContext,
    specs: &[String],
) -> Vec<(String, Result<ActionResult>)> {
    specs
        .iter()
        .map(|spec| {
            (
                spec.clone(),
                run_action_with_context(workspace_root, context, spec),
            )
        })
        .collect()
}

pub(crate) fn run_actions_with_context(
    workspace_root: &Path,
    context: &ActionContext,
    specs: &[String],
) -> Result<Vec<(String, ActionResult)>> {
    let mut results = Vec::with_capacity(specs.len());
    for (spec, result) in evaluate_actions_with_context(workspace_root, context, specs) {
        results.push((spec, result?));
    }
    Ok(results)
}

/// Check if all results passed.
pub fn all_passed(results: &[(String, ActionResult)]) -> bool {
    results.iter().all(|(_, r)| r.is_pass())
}

/// Collect failure reasons from results.
pub fn failure_reasons(results: &[(String, ActionResult)]) -> Vec<String> {
    results
        .iter()
        .filter_map(|(spec, r)| match r {
            ActionResult::Fail { reason } => Some(format!("{spec}: {reason}")),
            ActionResult::Pass | ActionResult::Notify { .. } => None,
        })
        .collect()
}

fn parse_spec(spec: &str) -> Result<(&str, &str)> {
    let (kind, arg) = spec
        .split_once(':')
        .ok_or_else(|| eyre!("invalid action spec (expected kind:arg): {spec}"))?;
    Ok((kind, arg))
}

fn resolve_glob(workspace_root: &Path, pattern: &str) -> Result<Vec<PathBuf>> {
    let full_pattern = if Path::new(pattern).is_absolute() {
        pattern.to_string()
    } else {
        workspace_root.join(pattern).to_string_lossy().to_string()
    };
    let canonical_root = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    let mut matches = Vec::new();
    for entry in glob(&full_pattern).map_err(|e| eyre!("invalid glob pattern {pattern}: {e}"))? {
        match entry {
            Ok(path) => {
                // Prevent path traversal — only include paths within workspace_root.
                let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
                if canonical.starts_with(&canonical_root) {
                    matches.push(path);
                } else {
                    warn!(
                        path = %path.display(),
                        workspace = %workspace_root.display(),
                        "glob match outside workspace root, skipping"
                    );
                }
            }
            Err(e) => warn!("glob walk error for {pattern}: {e}"),
        }
    }
    Ok(matches)
}

fn action_file_exists(
    workspace_root: &Path,
    context: &ActionContext,
    pattern: &str,
) -> Result<ActionResult> {
    let matches = context.resolve_targets(workspace_root, pattern)?;
    if matches.is_empty() {
        Ok(ActionResult::Fail {
            reason: format!("no files match pattern: {pattern}"),
        })
    } else if matches.iter().any(|path| !path.exists()) {
        Ok(ActionResult::Fail {
            reason: format!("missing file for pattern: {pattern}"),
        })
    } else {
        info!(pattern, count = matches.len(), "file_exists check passed");
        Ok(ActionResult::Pass)
    }
}

fn action_file_size_min(
    workspace_root: &Path,
    context: &ActionContext,
    arg: &str,
) -> Result<ActionResult> {
    // Format: glob_pattern:min_bytes
    let (pattern, min_str) = arg
        .rsplit_once(':')
        .ok_or_else(|| eyre!("file_size_min requires pattern:min_bytes, got: {arg}"))?;

    let min_bytes: u64 = min_str
        .parse()
        .map_err(|_| eyre!("file_size_min: invalid byte count: {min_str}"))?;

    let matches = context.resolve_targets(workspace_root, pattern)?;
    if matches.is_empty() {
        return Ok(ActionResult::Fail {
            reason: format!("no files match pattern: {pattern}"),
        });
    }

    for path in &matches {
        let meta =
            std::fs::metadata(path).map_err(|e| eyre!("cannot stat {}: {e}", path.display()))?;
        if meta.len() < min_bytes {
            return Ok(ActionResult::Fail {
                reason: format!(
                    "{} is {} bytes, minimum is {min_bytes}",
                    path.display(),
                    meta.len()
                ),
            });
        }
    }

    Ok(ActionResult::Pass)
}

fn action_cleanup(
    workspace_root: &Path,
    context: &ActionContext,
    pattern: &str,
) -> Result<ActionResult> {
    let matches = context.resolve_targets(workspace_root, pattern)?;
    let mut removed = 0;
    for path in matches {
        if path.is_file() {
            if let Err(e) = std::fs::remove_file(&path) {
                warn!("cleanup: failed to remove {}: {e}", path.display());
            } else {
                removed += 1;
            }
        } else if path.is_dir() {
            if let Err(e) = std::fs::remove_dir_all(&path) {
                warn!("cleanup: failed to remove dir {}: {e}", path.display());
            } else {
                removed += 1;
            }
        }
    }
    info!(pattern, removed, "cleanup action completed");
    // Cleanup always passes — missing files are fine
    Ok(ActionResult::Pass)
}

fn action_notify_user(message: &str) -> Result<ActionResult> {
    info!(message, "notify_user action");
    Ok(ActionResult::Notify {
        message: message.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_pass_when_file_exists() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("output.mp3"), b"audio data").unwrap();

        let result = run_action(temp.path(), "file_exists:output.mp3").unwrap();
        assert_eq!(result, ActionResult::Pass);
    }

    #[test]
    fn should_fail_when_file_missing() {
        let temp = tempfile::tempdir().unwrap();

        let result = run_action(temp.path(), "file_exists:output.mp3").unwrap();
        assert!(matches!(result, ActionResult::Fail { .. }));
    }

    #[test]
    fn should_match_glob_pattern() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("output")).unwrap();
        std::fs::write(temp.path().join("output/deck.pptx"), b"slides").unwrap();

        let result = run_action(temp.path(), "file_exists:output/*.pptx").unwrap();
        assert_eq!(result, ActionResult::Pass);
    }

    #[test]
    fn should_check_file_size_minimum() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("audio.mp3"), b"x").unwrap();

        let result = run_action(temp.path(), "file_size_min:audio.mp3:1024").unwrap();
        assert!(matches!(result, ActionResult::Fail { .. }));

        std::fs::write(temp.path().join("audio.mp3"), vec![0u8; 2048]).unwrap();
        let result = run_action(temp.path(), "file_size_min:audio.mp3:1024").unwrap();
        assert_eq!(result, ActionResult::Pass);
    }

    #[test]
    fn should_cleanup_matching_files() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("temp")).unwrap();
        std::fs::write(temp.path().join("temp/tts_1.wav"), b"data").unwrap();
        std::fs::write(temp.path().join("temp/tts_2.wav"), b"data").unwrap();

        let result = run_action(temp.path(), "cleanup:temp/tts_*").unwrap();
        assert_eq!(result, ActionResult::Pass);
        assert!(!temp.path().join("temp/tts_1.wav").exists());
        assert!(!temp.path().join("temp/tts_2.wav").exists());
    }

    #[test]
    fn should_pass_cleanup_when_no_files_match() {
        let temp = tempfile::tempdir().unwrap();
        let result = run_action(temp.path(), "cleanup:nonexistent_*").unwrap();
        assert_eq!(result, ActionResult::Pass);
    }

    #[test]
    fn should_return_notify_with_message() {
        let temp = tempfile::tempdir().unwrap();
        let result = run_action(temp.path(), "notify_user:TTS generation failed").unwrap();
        assert_eq!(
            result,
            ActionResult::Notify {
                message: "TTS generation failed".into()
            }
        );
        assert!(result.is_pass()); // Notify counts as pass
    }

    #[test]
    fn should_reject_unknown_action() {
        let temp = tempfile::tempdir().unwrap();
        let result = run_action(temp.path(), "unknown_action:arg");
        assert!(result.is_err());
    }

    #[test]
    fn should_reject_malformed_spec() {
        let temp = tempfile::tempdir().unwrap();
        let result = run_action(temp.path(), "no_colon_here");
        assert!(result.is_err());
    }

    #[test]
    fn should_extract_notifications_from_results() {
        let results = vec![
            ("file_exists:a.txt".into(), ActionResult::Pass),
            (
                "notify_user:done".into(),
                ActionResult::Notify {
                    message: "done".into(),
                },
            ),
            (
                "notify_user:ready".into(),
                ActionResult::Notify {
                    message: "ready".into(),
                },
            ),
        ];
        let notifs = notifications(&results);
        assert_eq!(notifs, vec!["done", "ready"]);
    }

    #[test]
    fn should_resolve_named_targets_for_file_checks() {
        let temp = tempfile::tempdir().unwrap();
        let artifact = temp.path().join("artifact.mp3");
        std::fs::write(&artifact, vec![0u8; 2048]).unwrap();

        let context = ActionContext::default().with_named_target("$artifact", vec![artifact]);
        let result =
            run_action_with_context(temp.path(), &context, "file_size_min:$artifact:1024").unwrap();

        assert_eq!(result, ActionResult::Pass);
    }

    #[test]
    fn should_resolve_multiple_named_targets_for_file_checks() {
        let temp = tempfile::tempdir().unwrap();
        let report = temp.path().join("report.md");
        let audio = temp.path().join("audio.mp3");
        std::fs::write(&report, b"report").unwrap();
        std::fs::write(&audio, vec![0u8; 2048]).unwrap();

        let context = ActionContext::default()
            .with_named_targets([("$report", vec![report]), ("$audio", vec![audio])]);

        let report_result =
            run_action_with_context(temp.path(), &context, "file_exists:$report").unwrap();
        let audio_result =
            run_action_with_context(temp.path(), &context, "file_size_min:$audio:1024").unwrap();

        assert_eq!(report_result, ActionResult::Pass);
        assert_eq!(audio_result, ActionResult::Pass);
    }

    #[test]
    fn should_support_absolute_patterns() {
        let temp = tempfile::tempdir().unwrap();
        let artifact = temp.path().join("absolute.mp3");
        std::fs::write(&artifact, b"audio").unwrap();

        let spec = format!("file_exists:{}", artifact.display());
        let result = run_action(temp.path(), &spec).unwrap();

        assert_eq!(result, ActionResult::Pass);
    }

    #[test]
    fn should_reject_path_traversal_in_cleanup() {
        let temp = tempfile::tempdir().unwrap();
        // Create a file outside the "workspace" subdirectory
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let outside_file = temp.path().join("secret.txt");
        std::fs::write(&outside_file, b"sensitive").unwrap();

        // Try to clean up ../secret.txt — should be blocked
        let result = run_action(&workspace, "cleanup:../secret.txt").unwrap();
        assert_eq!(result, ActionResult::Pass); // cleanup always passes
        // But the file outside workspace must NOT be deleted
        assert!(outside_file.exists(), "path traversal should be blocked");
    }

    #[test]
    fn should_run_multiple_actions_and_collect_results() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("output.mp3"), b"audio").unwrap();

        let specs = vec![
            "file_exists:output.mp3".to_string(),
            "file_exists:missing.txt".to_string(),
            "notify_user:done".to_string(),
        ];

        let results = run_actions(temp.path(), &specs).unwrap();
        assert_eq!(results.len(), 3);
        assert!(results[0].1.is_pass());
        assert!(!results[1].1.is_pass());
        assert!(results[2].1.is_pass());

        assert!(!all_passed(&results));
        let failures = failure_reasons(&results);
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("missing.txt"));
    }

    #[test]
    fn shared_validator_semantics_should_evaluate_actions_with_context_without_short_circuiting() {
        let temp = tempfile::tempdir().unwrap();
        let artifact = temp.path().join("artifact.mp3");
        std::fs::write(&artifact, vec![0u8; 2048]).unwrap();

        let context = ActionContext::default().with_named_target("$artifact", vec![artifact]);
        let specs = vec![
            "file_exists:$artifact".to_string(),
            "file_size_min:$artifact:1024".to_string(),
            "file_exists:missing.txt".to_string(),
        ];

        let results = evaluate_actions_with_context(temp.path(), &context, &specs);
        assert_eq!(results.len(), 3);
        assert!(matches!(results[0].1, Ok(ActionResult::Pass)));
        assert!(matches!(results[1].1, Ok(ActionResult::Pass)));
        assert!(matches!(results[2].1, Ok(ActionResult::Fail { .. })));
    }
}
