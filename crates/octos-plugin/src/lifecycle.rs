//! Hardware lifecycle executor for plugin manifests.
//!
//! Runs ordered lifecycle phases (preflight, init, ready_check, shutdown,
//! emergency_shutdown) with per-step timeout, retry, and critical-failure
//! abort semantics. Every step is dispatched through the [`Sandbox`] trait,
//! with [`BLOCKED_ENV_VARS`] scrubbed from the child environment and the
//! command string screened by [`is_safe_shell_command`] before dispatch.
//!
//! # Why this file looks paranoid
//!
//! An earlier revision ran commands with `tokio::process::Command::new("sh")
//! .arg("-c").arg(step.command)` directly. That path had no env sanitization,
//! no deny list, no sandbox isolation, and relied on `tokio::time::timeout`
//! alone — which leaves the child running after the handle is dropped on
//! Unix. This executor closes every one of those gaps:
//!
//! - Steps run through a [`Sandbox`] implementation (bwrap on Linux,
//!   sandbox-exec on macOS, `cmd /C` on Windows, pass-through otherwise).
//! - [`BLOCKED_ENV_VARS`] is applied to every spawned child so code
//!   injection vectors like `LD_PRELOAD`, `NODE_OPTIONS`, and `BASH_ENV`
//!   are scrubbed.
//! - [`is_safe_shell_command`] denies obvious footguns (`rm -rf /`, `dd`,
//!   `mkfs`, fork bombs) before we even spawn a process.
//! - On timeout we explicitly `kill().await` the child AND rely on
//!   `kill_on_drop(true)` as a belt-and-braces safety net — the test
//!   `should_kill_child_when_step_timeout_exceeded` asserts the PID is dead
//!   within 500ms of the timeout firing.

use std::path::Path;
use std::time::Duration;

use eyre::Result;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

/// Environment variables blocked inside lifecycle step execution.
///
/// Mirrors `octos-agent/src/sandbox/mod.rs::BLOCKED_ENV_VARS`. The two lists
/// MUST stay in sync — the `blocked_env_vars_match_agent_sandbox` test in
/// `tests/lifecycle_sandbox.rs` compiles the agent source in and asserts
/// the two lists are element-wise equal. If you add/remove a variable here,
/// update the agent source too (and vice versa).
pub const BLOCKED_ENV_VARS: &[&str] = &[
    // Linux: shared library injection
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "LD_AUDIT",
    // macOS: dylib injection
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "DYLD_FRAMEWORK_PATH",
    "DYLD_FALLBACK_LIBRARY_PATH",
    "DYLD_VERSIONED_LIBRARY_PATH",
    // Runtime-specific code injection
    "NODE_OPTIONS",
    "PYTHONSTARTUP",
    "PYTHONPATH",
    "PERL5OPT",
    "RUBYOPT",
    "RUBYLIB",
    "JAVA_TOOL_OPTIONS",
    // Shell startup injection
    "BASH_ENV",
    "ENV",
    "ZDOTDIR",
];

/// A single step in a hardware lifecycle phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LifecycleStep {
    /// Human-readable label for logging.
    pub label: String,
    /// Shell command to execute.
    pub command: String,
    /// Timeout for this step in milliseconds. Kept in milliseconds rather
    /// than seconds so tests can exercise sub-second timeouts without
    /// sleeping for real wallclock seconds.
    #[serde(default = "default_timeout_ms", alias = "timeout_ms")]
    pub timeout_ms: u64,
    /// Number of retry attempts on failure.
    #[serde(default)]
    pub retries: u32,
    /// If true, failure of this step aborts the entire phase.
    #[serde(default = "default_true")]
    pub critical: bool,
}

fn default_timeout_ms() -> u64 {
    30_000
}

fn default_true() -> bool {
    true
}

/// The five lifecycle phases declared by a [`HardwareLifecycle`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecyclePhase {
    Preflight,
    Init,
    ReadyCheck,
    Shutdown,
    EmergencyShutdown,
}

impl LifecyclePhase {
    /// Stable, lowercase label used in logs and metric labels.
    pub fn as_str(&self) -> &'static str {
        match self {
            LifecyclePhase::Preflight => "preflight",
            LifecyclePhase::Init => "init",
            LifecyclePhase::ReadyCheck => "ready_check",
            LifecyclePhase::Shutdown => "shutdown",
            LifecyclePhase::EmergencyShutdown => "emergency_shutdown",
        }
    }
}

impl std::fmt::Display for LifecyclePhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Hardware lifecycle declaration for a plugin.
///
/// Each phase is a list of steps executed in order. Phases are:
/// - `preflight`: Checks before initialization (sensors connected, firmware OK)
/// - `init`: Bring hardware to operational state
/// - `ready_check`: Verify hardware is ready for operation
/// - `shutdown`: Graceful shutdown sequence
/// - `emergency_shutdown`: Fast shutdown (minimal steps, short timeouts)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HardwareLifecycle {
    #[serde(default)]
    pub preflight: Vec<LifecycleStep>,
    #[serde(default)]
    pub init: Vec<LifecycleStep>,
    #[serde(default)]
    pub ready_check: Vec<LifecycleStep>,
    #[serde(default)]
    pub shutdown: Vec<LifecycleStep>,
    #[serde(default)]
    pub emergency_shutdown: Vec<LifecycleStep>,
}

/// Reason a step was killed, recorded by the executor for observability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKillReason {
    /// Step exceeded its `timeout_ms` budget.
    Timeout,
    /// Step failed and was `critical=true`, aborting the phase.
    CriticalFailure,
    /// Command was denied by [`is_safe_shell_command`] before dispatch.
    SandboxDeny,
}

impl StepKillReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            StepKillReason::Timeout => "timeout",
            StepKillReason::CriticalFailure => "critical_failure",
            StepKillReason::SandboxDeny => "sandbox_deny",
        }
    }
}

/// Outcome label used in the `octos_lifecycle_step_total` counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome {
    Ok,
    Failed,
    Denied,
    TimedOut,
}

impl StepOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            StepOutcome::Ok => "ok",
            StepOutcome::Failed => "failed",
            StepOutcome::Denied => "denied",
            StepOutcome::TimedOut => "timed_out",
        }
    }
}

/// Result of executing a single lifecycle step.
#[derive(Debug, Clone)]
pub struct StepResult {
    pub label: String,
    pub outcome: StepOutcome,
    pub error: Option<String>,
}

/// Result of executing a lifecycle phase.
#[derive(Debug, Clone)]
pub struct PhaseResult {
    pub phase: String,
    pub steps_completed: usize,
    pub steps_total: usize,
    pub success: bool,
    pub error: Option<String>,
    pub steps: Vec<StepResult>,
}

/// Wraps a shell command into a sandboxed [`Command`].
///
/// Mirrors `octos_agent::sandbox::Sandbox` but lives here so the plugin SDK
/// doesn't need a runtime dependency on the agent crate. Implementations
/// MUST NOT spawn the child themselves — the executor spawns the returned
/// [`Command`] so it can attach the required env sanitization and
/// `kill_on_drop` flag before spawn.
pub trait Sandbox: Send + Sync {
    /// Wrap a shell command string into a [`Command`] ready to spawn.
    fn wrap_command(&self, shell_command: &str, cwd: &Path) -> Command;
}

/// No-op sandbox: dispatches via `sh -c` on Unix, `cmd /C` on Windows.
///
/// Cross-platform by design so Windows hardware skills still get SafePolicy
/// + BLOCKED_ENV_VARS scrubbing even when no platform sandbox is available.
pub struct NoSandbox;

impl Sandbox for NoSandbox {
    fn wrap_command(&self, shell_command: &str, cwd: &Path) -> Command {
        #[cfg(windows)]
        {
            let mut cmd = Command::new("cmd");
            cmd.arg("/C").arg(shell_command).current_dir(cwd);
            cmd
        }
        #[cfg(not(windows))]
        {
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(shell_command).current_dir(cwd);
            cmd
        }
    }
}

/// Error returned when a command fails [`is_safe_shell_command`].
#[derive(Debug, Clone)]
pub struct SafePolicyDenial {
    /// The dangerous pattern that matched.
    pub pattern: String,
}

impl std::fmt::Display for SafePolicyDenial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "command denied by SafePolicy: matched pattern '{}'",
            self.pattern
        )
    }
}

impl std::error::Error for SafePolicyDenial {}

/// Mirrors `SafePolicy::default().check()` deny patterns from
/// `octos_agent::policy`. See `octos-agent/src/policy.rs` for the full
/// rationale. The deny list here is intentionally short: `rm -rf /`,
/// `dd if=`, `mkfs`, fork bomb, `chmod -R 777 /`. Defense in depth, not a
/// security boundary — real isolation comes from the [`Sandbox`] layer.
///
/// Patterns are matched on the whitespace-normalized command string at
/// word boundaries, so `mkfs` will not match inside `unmkfsblah`.
pub fn is_safe_shell_command(command: &str) -> std::result::Result<(), SafePolicyDenial> {
    const DENY_PATTERNS: &[&str] = &[
        "rm -rf /",
        "rm -rf /*",
        "dd if=",
        "mkfs",
        ":(){:|:&};:", // fork bomb
        "chmod -R 777 /",
    ];

    let normalized = normalize_whitespace(command);
    for pattern in DENY_PATTERNS {
        if contains_at_word_boundary(&normalized, pattern) {
            return Err(SafePolicyDenial {
                pattern: (*pattern).to_string(),
            });
        }
    }
    Ok(())
}

/// Collapse consecutive whitespace into single spaces and trim.
fn normalize_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Check if `pattern` appears in `haystack` at a word boundary.
fn contains_at_word_boundary(haystack: &str, pattern: &str) -> bool {
    let pat = pattern.as_bytes();
    let hay = haystack.as_bytes();
    if pat.len() > hay.len() {
        return false;
    }
    for i in 0..=(hay.len() - pat.len()) {
        if &hay[i..i + pat.len()] == pat {
            let left_ok = i == 0 || !hay[i - 1].is_ascii_alphanumeric();
            let right_ok =
                i + pat.len() == hay.len() || !hay[i + pat.len()].is_ascii_alphanumeric();
            if left_ok && right_ok {
                return true;
            }
        }
    }
    false
}

/// Executes lifecycle phases with timeout, retry, and sandbox isolation.
pub struct LifecycleExecutor {
    sandbox: Box<dyn Sandbox>,
    cwd: std::path::PathBuf,
}

impl LifecycleExecutor {
    /// Construct an executor with the provided sandbox and working directory.
    pub fn new(sandbox: Box<dyn Sandbox>, cwd: impl Into<std::path::PathBuf>) -> Self {
        Self {
            sandbox,
            cwd: cwd.into(),
        }
    }

    /// Construct an executor backed by [`NoSandbox`] rooted at the OS temp dir.
    ///
    /// Convenient for tests and examples; production callers should pass a
    /// platform sandbox via [`LifecycleExecutor::new`].
    pub fn with_no_sandbox() -> Self {
        Self::new(Box::new(NoSandbox), std::env::temp_dir())
    }

    /// Run a lifecycle phase (list of steps) to completion.
    ///
    /// Executes steps in order. On failure:
    /// - Retries up to `step.retries` times
    /// - If `step.critical` is true, aborts the phase on final failure
    /// - Non-critical steps log a warning and continue
    pub async fn run_phase(&self, phase: LifecyclePhase, steps: &[LifecycleStep]) -> PhaseResult {
        let phase_name = phase.as_str();
        let total = steps.len();
        let mut step_results = Vec::with_capacity(total);

        for (i, step) in steps.iter().enumerate() {
            let mut last_error: Option<String> = None;
            let mut last_outcome = StepOutcome::Failed;

            // SafePolicy check is not retried: a denied command stays denied.
            if let Err(denial) = is_safe_shell_command(&step.command) {
                metrics::counter!(
                    "octos_lifecycle_step_total",
                    "phase" => phase_name,
                    "outcome" => StepOutcome::Denied.as_str()
                )
                .increment(1);
                metrics::counter!(
                    "octos_lifecycle_step_killed_total",
                    "phase" => phase_name,
                    "reason" => StepKillReason::SandboxDeny.as_str()
                )
                .increment(1);
                tracing::error!(
                    phase = phase_name,
                    step = step.label,
                    pattern = denial.pattern,
                    "lifecycle step denied by SafePolicy"
                );
                let err_msg = format!("{denial}");
                step_results.push(StepResult {
                    label: step.label.clone(),
                    outcome: StepOutcome::Denied,
                    error: Some(err_msg.clone()),
                });
                if step.critical {
                    return PhaseResult {
                        phase: phase_name.to_string(),
                        steps_completed: i,
                        steps_total: total,
                        success: false,
                        error: Some(format!("{}: {}", step.label, err_msg)),
                        steps: step_results,
                    };
                }
                continue;
            }

            for attempt in 0..=step.retries {
                if attempt > 0 {
                    tracing::warn!(
                        phase = phase_name,
                        step = step.label,
                        attempt = attempt + 1,
                        max = step.retries + 1,
                        "retrying lifecycle step"
                    );
                }

                match self.run_step(phase, step).await {
                    Ok(()) => {
                        last_error = None;
                        last_outcome = StepOutcome::Ok;
                        break;
                    }
                    Err(StepError::TimedOut(e)) => {
                        last_error = Some(e);
                        last_outcome = StepOutcome::TimedOut;
                    }
                    Err(StepError::Failed(e)) => {
                        last_error = Some(e);
                        last_outcome = StepOutcome::Failed;
                    }
                }
            }

            metrics::counter!(
                "octos_lifecycle_step_total",
                "phase" => phase_name,
                "outcome" => last_outcome.as_str()
            )
            .increment(1);

            match last_error {
                None => step_results.push(StepResult {
                    label: step.label.clone(),
                    outcome: StepOutcome::Ok,
                    error: None,
                }),
                Some(err) => {
                    step_results.push(StepResult {
                        label: step.label.clone(),
                        outcome: last_outcome,
                        error: Some(err.clone()),
                    });
                    if step.critical {
                        metrics::counter!(
                            "octos_lifecycle_step_killed_total",
                            "phase" => phase_name,
                            "reason" => StepKillReason::CriticalFailure.as_str()
                        )
                        .increment(1);
                        tracing::error!(
                            phase = phase_name,
                            step = step.label,
                            error = %err,
                            "critical lifecycle step failed, aborting phase"
                        );
                        return PhaseResult {
                            phase: phase_name.to_string(),
                            steps_completed: i,
                            steps_total: total,
                            success: false,
                            error: Some(format!("{}: {}", step.label, err)),
                            steps: step_results,
                        };
                    }
                    tracing::warn!(
                        phase = phase_name,
                        step = step.label,
                        error = %err,
                        "non-critical lifecycle step failed, continuing"
                    );
                }
            }
        }

        PhaseResult {
            phase: phase_name.to_string(),
            steps_completed: total,
            steps_total: total,
            success: true,
            error: None,
            steps: step_results,
        }
    }

    /// Dispatch a single step through the sandbox with timeout enforcement.
    ///
    /// The child is spawned into its own process group (Unix) with
    /// `kill_on_drop(true)` as a belt-and-braces safety net. On timeout
    /// we kill the ENTIRE process group (via `kill -9 -$pgid`) so that
    /// background children spawned by the shell (e.g. `sleep 30 &`) are
    /// reaped along with their parent. Without this, `tokio::process::
    /// Child::kill` only sends SIGKILL to the direct child, leaving
    /// orphans re-parented to init/launchd.
    async fn run_step(&self, phase: LifecyclePhase, step: &LifecycleStep) -> StepResult2 {
        let timeout = Duration::from_millis(step.timeout_ms);
        let mut cmd = self.sandbox.wrap_command(&step.command, &self.cwd);

        // Scrub injection-vector env vars from the child. Applies regardless
        // of sandbox backend — belt and braces.
        for var in BLOCKED_ENV_VARS {
            cmd.env_remove(var);
        }
        cmd.kill_on_drop(true);

        // On Unix, put the child into its own process group so we can
        // SIGKILL the entire group (including any background children
        // forked by `sh -c`). `process_group(0)` is safe: it uses
        // posix_spawn/fork+setpgid under the hood with no UB potential.
        #[cfg(unix)]
        cmd.process_group(0);

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return Err(StepError::Failed(format!(
                    "failed to spawn step '{}': {e}",
                    step.label
                )));
            }
        };

        let pid = child.id();

        match tokio::time::timeout(timeout, child.wait()).await {
            Ok(Ok(status)) => {
                if status.success() {
                    Ok(())
                } else {
                    Err(StepError::Failed(format!(
                        "step '{}' exited with {status}",
                        step.label
                    )))
                }
            }
            Ok(Err(e)) => Err(StepError::Failed(format!(
                "step '{}' wait error: {e}",
                step.label
            ))),
            Err(_) => {
                // Timeout fired. Kill the direct child AND (on Unix) the
                // entire process group so orphaned background children
                // are reaped. kill_on_drop covers the direct child if we
                // error out before reaching child.kill() below.
                #[cfg(unix)]
                if let Some(pid) = pid {
                    // `kill -9 -<pgid>` targets the whole group. Since
                    // we called process_group(0), pid == pgid.
                    let _ = std::process::Command::new("kill")
                        .args(["-9", &format!("-{pid}")])
                        .status();
                }
                if let Err(e) = child.kill().await {
                    tracing::warn!(
                        phase = phase.as_str(),
                        step = step.label,
                        error = %e,
                        "failed to kill timed-out lifecycle step child"
                    );
                }
                #[cfg(windows)]
                if let Some(pid) = pid {
                    // On Windows, TerminateProcess reaches only the
                    // direct child. Use taskkill /T to kill the tree.
                    let _ = std::process::Command::new("taskkill")
                        .args(["/F", "/T", "/PID", &pid.to_string()])
                        .status();
                }
                metrics::counter!(
                    "octos_lifecycle_step_killed_total",
                    "phase" => phase.as_str(),
                    "reason" => StepKillReason::Timeout.as_str()
                )
                .increment(1);
                Err(StepError::TimedOut(format!(
                    "step '{}' timed out after {}ms",
                    step.label, step.timeout_ms
                )))
            }
        }
    }
}

type StepResult2 = std::result::Result<(), StepError>;

#[derive(Debug)]
enum StepError {
    /// Wallclock budget exceeded; child was killed.
    TimedOut(String),
    /// Command failed for any other reason (non-zero exit, spawn error).
    Failed(String),
}

/// Backwards-compatible alias. Older callers used `Result<()>` without the
/// [`StepError`] discriminant; keep the alias so they compile unchanged.
#[doc(hidden)]
pub type LifecycleResult = Result<()>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_parse_lifecycle_with_all_phases() {
        let json = r#"{
            "preflight": [
                {"label": "check_sensor", "command": "echo ok", "timeout_ms": 5000, "retries": 1, "critical": true}
            ],
            "init": [
                {"label": "power_on", "command": "echo on"}
            ],
            "shutdown": [],
            "emergency_shutdown": [
                {"label": "e_stop", "command": "echo stop", "timeout_ms": 2000}
            ]
        }"#;
        let lifecycle: HardwareLifecycle = serde_json::from_str(json).unwrap();
        assert_eq!(lifecycle.preflight.len(), 1);
        assert_eq!(lifecycle.init.len(), 1);
        assert!(lifecycle.shutdown.is_empty());
        assert_eq!(lifecycle.emergency_shutdown.len(), 1);
        assert_eq!(lifecycle.emergency_shutdown[0].timeout_ms, 2000);
    }

    #[test]
    fn should_parse_lifecycle_without_optional_phases() {
        let json = "{}";
        let lifecycle: HardwareLifecycle = serde_json::from_str(json).unwrap();
        assert!(lifecycle.preflight.is_empty());
        assert!(lifecycle.init.is_empty());
    }

    #[test]
    fn should_deny_dangerous_commands_via_safepolicy() {
        assert!(is_safe_shell_command("rm -rf /").is_err());
        assert!(is_safe_shell_command("dd if=/dev/zero of=/dev/sda").is_err());
        assert!(is_safe_shell_command("mkfs /dev/sda1").is_err());
        assert!(is_safe_shell_command("chmod -R 777 /").is_err());
        assert!(is_safe_shell_command(":(){:|:&};:").is_err());
        // Word-boundary: innocuous string that contains "mkfs" inside a
        // larger word must NOT match.
        assert!(is_safe_shell_command("echo unmkfsblah").is_ok());
        assert!(is_safe_shell_command("echo hello").is_ok());
    }

    #[test]
    fn should_detect_safepolicy_denial_with_mangled_whitespace() {
        // Double-space / tab variants must still be caught.
        assert!(is_safe_shell_command("rm  -rf  /").is_err());
        assert!(is_safe_shell_command("rm\t-rf\t/").is_err());
    }

    #[test]
    fn lifecycle_phase_as_str_stable() {
        assert_eq!(LifecyclePhase::Preflight.as_str(), "preflight");
        assert_eq!(LifecyclePhase::Init.as_str(), "init");
        assert_eq!(LifecyclePhase::ReadyCheck.as_str(), "ready_check");
        assert_eq!(LifecyclePhase::Shutdown.as_str(), "shutdown");
        assert_eq!(
            LifecyclePhase::EmergencyShutdown.as_str(),
            "emergency_shutdown"
        );
    }

    #[test]
    fn step_kill_reason_labels_stable() {
        assert_eq!(StepKillReason::Timeout.as_str(), "timeout");
        assert_eq!(StepKillReason::CriticalFailure.as_str(), "critical_failure");
        assert_eq!(StepKillReason::SandboxDeny.as_str(), "sandbox_deny");
    }

    #[test]
    fn blocked_env_vars_contains_critical() {
        for var in [
            "LD_PRELOAD",
            "DYLD_INSERT_LIBRARIES",
            "NODE_OPTIONS",
            "BASH_ENV",
        ] {
            assert!(BLOCKED_ENV_VARS.contains(&var), "missing {var}");
        }
    }
}
