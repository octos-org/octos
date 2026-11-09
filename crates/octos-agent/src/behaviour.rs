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
use tracing::{debug, info, warn};

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
/// - `file_count_eq:<glob>:<count>` — exactly N files match the glob or named target
/// - `file_count_min:<glob>:<count>` — at least N files match the glob or named target
/// - `file_count_max:<glob>:<count>` — at most N files match the glob or named target
/// - `any_exists:<glob>|<glob>|...` — at least one target resolves to an existing file
/// - `all_exist:<glob>|<glob>|...` — every listed target resolves to an existing file
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
        "file_count_eq" => action_file_count_eq(workspace_root, context, arg),
        "file_count_min" => action_file_count_min(workspace_root, context, arg),
        "file_count_max" => action_file_count_max(workspace_root, context, arg),
        "any_exists" => action_any_exists(workspace_root, context, arg),
        "all_exist" => action_all_exist(workspace_root, context, arg),
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
        // This check is polled frequently by long-running workers, so keep the
        // success signal off the default info path.
        debug!(pattern, count = matches.len(), "file_exists check passed");
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
            // Report the spec-relative path with `/` separators so the reason
            // is deterministic across platforms (and does not leak the
            // absolute workspace path). On Windows `path.display()` would use
            // `\`, breaking downstream `contains("dir/file")` checks.
            let display = path
                .strip_prefix(workspace_root)
                .unwrap_or(path.as_path())
                .to_string_lossy()
                .replace('\\', "/");
            return Ok(ActionResult::Fail {
                reason: format!("{display} is {} bytes, minimum is {min_bytes}", meta.len()),
            });
        }
    }

    Ok(ActionResult::Pass)
}

fn action_file_count_eq(
    workspace_root: &Path,
    context: &ActionContext,
    arg: &str,
) -> Result<ActionResult> {
    let (pattern, expected) = parse_count_spec("file_count_eq", arg)?;
    let count = count_existing_files(workspace_root, context, pattern)?;
    if count == expected {
        info!(pattern, count, expected, "file_count_eq check passed");
        Ok(ActionResult::Pass)
    } else {
        Ok(ActionResult::Fail {
            reason: format!("{pattern} matched {count} files, expected {expected}"),
        })
    }
}

fn action_file_count_min(
    workspace_root: &Path,
    context: &ActionContext,
    arg: &str,
) -> Result<ActionResult> {
    let (pattern, minimum) = parse_count_spec("file_count_min", arg)?;
    let count = count_existing_files(workspace_root, context, pattern)?;
    if count >= minimum {
        info!(pattern, count, minimum, "file_count_min check passed");
        Ok(ActionResult::Pass)
    } else {
        Ok(ActionResult::Fail {
            reason: format!("{pattern} matched {count} files, minimum is {minimum}"),
        })
    }
}

fn action_file_count_max(
    workspace_root: &Path,
    context: &ActionContext,
    arg: &str,
) -> Result<ActionResult> {
    let (pattern, maximum) = parse_count_spec("file_count_max", arg)?;
    let count = count_existing_files(workspace_root, context, pattern)?;
    if count <= maximum {
        info!(pattern, count, maximum, "file_count_max check passed");
        Ok(ActionResult::Pass)
    } else {
        Ok(ActionResult::Fail {
            reason: format!("{pattern} matched {count} files, maximum is {maximum}"),
        })
    }
}

fn action_any_exists(
    workspace_root: &Path,
    context: &ActionContext,
    arg: &str,
) -> Result<ActionResult> {
    let targets = split_target_list(arg);
    if targets.is_empty() {
        return Err(eyre!("any_exists requires at least one target"));
    }

    for target in &targets {
        if !resolve_existing_files(workspace_root, context, target)?.is_empty() {
            info!(target, "any_exists check passed");
            return Ok(ActionResult::Pass);
        }
    }

    Ok(ActionResult::Fail {
        reason: format!("none of the targets exist: {}", targets.join("|")),
    })
}

fn action_all_exist(
    workspace_root: &Path,
    context: &ActionContext,
    arg: &str,
) -> Result<ActionResult> {
    let targets = split_target_list(arg);
    if targets.is_empty() {
        return Err(eyre!("all_exist requires at least one target"));
    }

    for target in &targets {
        let matches = resolve_existing_files(workspace_root, context, target)?;
        if matches.is_empty() {
            return Ok(ActionResult::Fail {
                reason: format!("missing target: {target}"),
            });
        }
    }

    info!(targets = %targets.join("|"), "all_exist check passed");
    Ok(ActionResult::Pass)
}

fn parse_count_spec<'a>(action: &'a str, arg: &'a str) -> Result<(&'a str, usize)> {
    let (pattern, count_str) = arg
        .rsplit_once(':')
        .ok_or_else(|| eyre!("{action} requires pattern:count, got: {arg}"))?;
    let count: usize = count_str
        .parse()
        .map_err(|_| eyre!("{action}: invalid count: {count_str}"))?;
    Ok((pattern, count))
}

fn resolve_existing_files(
    workspace_root: &Path,
    context: &ActionContext,
    target: &str,
) -> Result<Vec<PathBuf>> {
    Ok(context
        .resolve_targets(workspace_root, target)?
        .into_iter()
        .filter(|path| path.exists() && path.is_file())
        .collect())
}

fn count_existing_files(
    workspace_root: &Path,
    context: &ActionContext,
    target: &str,
) -> Result<usize> {
    Ok(resolve_existing_files(workspace_root, context, target)?.len())
}

fn split_target_list(arg: &str) -> Vec<&str> {
    arg.split('|')
        .map(str::trim)
        .filter(|target| !target.is_empty())
        .collect()
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
