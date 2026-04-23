//! Declarative validator runner (harness M4.3).
//!
//! Runs the typed validators declared in `WorkspacePolicy.validation.validators`
//! and produces durable typed outcomes that block terminal success for the
//! workspace contract when a required validator fails. Optional failures are
//! surfaced as warnings without blocking delivery.
//!
//! # Safety invariants
//!
//! - Command validators go through the shell-safety layer
//!   ([`crate::policy::SafePolicy`]) and strip [`BLOCKED_ENV_VARS`] before
//!   invoking a child, reusing the same sanitization as `ShellTool`. No
//!   `Command::new("sh")` escape hatch.
//! - Command validator timeouts kill the child process via SIGTERM -> SIGKILL
//!   on Unix and `taskkill /F /T` on Windows.
//! - Outcomes carry a stable `schema_version` (starting at 1) so persisted
//!   records replay across harness upgrades.
//! - Evidence files live under `<workspace_root>/.octos/validator-evidence/`
//!   to keep operator-visible logs durable without polluting the workspace.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use eyre::{Result, WrapErr, eyre};
use metrics::counter;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, warn};

use crate::policy::{CommandPolicy, Decision, SafePolicy};
use crate::subprocess_env::{EnvAllowlist, sanitize_command_env};
use crate::tools::{ToolRegistry, ToolResult};
use crate::workspace_policy::{Validator, ValidatorPhaseKind, ValidatorSpec};

/// Current schema version for [`ValidatorOutcome`] persistence.
pub const VALIDATOR_RESULT_SCHEMA_VERSION: u32 = 1;

const EVIDENCE_SUBDIR: &str = ".octos/validator-evidence";
const DEFAULT_COMMAND_TIMEOUT_MS: u64 = 30_000;
const MAX_EVIDENCE_BYTES: usize = 512 * 1024;
const KILL_GRACE_PERIOD: Duration = Duration::from_millis(300);

/// Phase in which a validator runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidatorPhase {
    TurnEnd,
    Completion,
}

impl ValidatorPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::TurnEnd => "turn_end",
            Self::Completion => "completion",
        }
    }
}

impl From<ValidatorPhaseKind> for ValidatorPhase {
    fn from(value: ValidatorPhaseKind) -> Self {
        match value {
            ValidatorPhaseKind::TurnEnd => Self::TurnEnd,
            ValidatorPhaseKind::Completion => Self::Completion,
        }
    }
}

/// Typed terminal status for a validator run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidatorStatus {
    /// Validator finished successfully.
    Pass,
    /// Validator ran to completion but reported a failure.
    Fail,
    /// Validator exceeded its timeout budget.
    Timeout,
    /// Validator could not run (policy deny, missing tool, etc.).
    Error,
}

impl ValidatorStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Timeout => "timeout",
            Self::Error => "error",
        }
    }
}

/// Invocation context shared by a batch of validators for the same workspace.
#[derive(Clone, Debug)]
pub struct ValidatorInvocation {
    pub phase: ValidatorPhase,
    pub workspace_root: PathBuf,
    pub repo_label: String,
}

/// Typed durable outcome of a single validator run.
///
/// Carries enough information to replay after reload or restart: the
/// validator id, typed status, human-readable reason, duration, evidence
/// path, stderr tail, and schema version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidatorOutcome {
    /// Schema version of this record. Starts at 1.
    pub schema_version: u32,
    pub validator_id: String,
    pub phase: ValidatorPhase,
    pub kind: String,
    pub repo_label: String,
    pub required: bool,
    pub status: ValidatorStatus,
    pub reason: String,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    pub started_at: DateTime<Utc>,
}

impl ValidatorOutcome {
    /// Does this outcome satisfy the required-gate contract?
    ///
    /// A required failure/timeout/error blocks terminal success. Optional
    /// validators never block the gate.
    pub fn required_gate_passed(&self) -> bool {
        if !self.required {
            return true;
        }
        matches!(self.status, ValidatorStatus::Pass)
    }
}

/// Append-only JSONL ledger that persists validator outcomes for replay.
#[derive(Clone, Debug)]
pub struct ValidatorLedger {
    path: Arc<PathBuf>,
}

impl ValidatorLedger {
    /// Open (or create) an append-only ledger at `path`. The parent directory
    /// is created on demand.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .wrap_err_with(|| format!("create ledger dir failed: {}", parent.display()))?;
        }
        Ok(Self {
            path: Arc::new(path),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append a single outcome record to the ledger.
    pub fn append(&self, outcome: &ValidatorOutcome) -> Result<()> {
        use std::fs::OpenOptions;
        use std::io::Write;

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path.as_ref())
            .wrap_err_with(|| format!("open ledger failed: {}", self.path.display()))?;
        let json = serde_json::to_string(outcome).wrap_err("serialize validator outcome failed")?;
        writeln!(file, "{json}")
            .wrap_err_with(|| format!("write ledger failed: {}", self.path.display()))?;
        Ok(())
    }

    /// Read every persisted outcome from the ledger (for replay).
    pub fn read_all(&self) -> Result<Vec<ValidatorOutcome>> {
        use std::fs::File;
        use std::io::{BufRead, BufReader};

        let file = match File::open(self.path.as_ref()) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(eyre!("open ledger failed: {}: {err}", self.path.display()));
            }
        };
        let mut outcomes = Vec::new();
        for line in BufReader::new(file).lines() {
            let line = line.wrap_err("read ledger line failed")?;
            if line.trim().is_empty() {
                continue;
            }
            let outcome: ValidatorOutcome = serde_json::from_str(&line)
                .wrap_err_with(|| format!("parse ledger line failed: {line}"))?;
            outcomes.push(outcome);
        }
        Ok(outcomes)
    }
}

/// Dispatches a ToolCall validator. Abstracts over the real `ToolRegistry`
/// so test harnesses and short-lived call sites can provide a lightweight
/// implementation without cloning the registry.
#[async_trait::async_trait]
pub trait ValidatorToolDispatcher: Send + Sync {
    async fn dispatch(&self, tool: &str, args: &serde_json::Value) -> Result<ToolResult>;
}

#[async_trait::async_trait]
impl ValidatorToolDispatcher for ToolRegistry {
    async fn dispatch(&self, tool: &str, args: &serde_json::Value) -> Result<ToolResult> {
        self.execute(tool, args).await
    }
}

/// Dispatcher that looks up tools from a pre-captured map of `Arc<dyn Tool>`.
///
/// Suitable for short-lived call sites that only hold a `&ToolRegistry`
/// reference but need a `ValidatorRunner` without cloning the full registry.
pub struct MapToolDispatcher {
    tools: std::collections::HashMap<String, std::sync::Arc<dyn crate::tools::Tool>>,
}

impl MapToolDispatcher {
    pub fn new() -> Self {
        Self {
            tools: std::collections::HashMap::new(),
        }
    }

    pub fn insert(
        &mut self,
        name: impl Into<String>,
        tool: std::sync::Arc<dyn crate::tools::Tool>,
    ) {
        self.tools.insert(name.into(), tool);
    }

    pub fn from_registry(registry: &ToolRegistry) -> Self {
        let mut me = Self::new();
        for name in registry.tool_names() {
            if let Some(tool) = registry.get_tool(&name) {
                me.insert(name, tool);
            }
        }
        me
    }
}

impl Default for MapToolDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl ValidatorToolDispatcher for MapToolDispatcher {
    async fn dispatch(&self, tool: &str, args: &serde_json::Value) -> Result<ToolResult> {
        let Some(handle) = self.tools.get(tool).cloned() else {
            return Err(eyre!("tool '{tool}' not registered for validator dispatch"));
        };
        handle.execute(args).await
    }
}

/// Runner that executes typed validators and produces durable outcomes.
#[derive(Clone)]
pub struct ValidatorRunner {
    dispatcher: Arc<dyn ValidatorToolDispatcher>,
    evidence_root: PathBuf,
    policy: Arc<dyn CommandPolicy>,
    ledger: Option<ValidatorLedger>,
}

impl std::fmt::Debug for ValidatorRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ValidatorRunner")
            .field("evidence_root", &self.evidence_root)
            .field("ledger", &self.ledger)
            .finish_non_exhaustive()
    }
}

impl ValidatorRunner {
    /// Create a runner bound to `tools` with evidence under
    /// `<workspace_root>/.octos/validator-evidence/`.
    pub fn new(tools: Arc<ToolRegistry>, workspace_root: impl Into<PathBuf>) -> Self {
        let dispatcher: Arc<dyn ValidatorToolDispatcher> = tools;
        Self::with_dispatcher(dispatcher, workspace_root)
    }

    /// Create a runner that dispatches tool validators through `dispatcher`.
    pub fn with_dispatcher(
        dispatcher: Arc<dyn ValidatorToolDispatcher>,
        workspace_root: impl Into<PathBuf>,
    ) -> Self {
        let evidence_root = workspace_root.into().join(EVIDENCE_SUBDIR);
        Self {
            dispatcher,
            evidence_root,
            policy: Arc::new(SafePolicy::default()),
            ledger: None,
        }
    }

    /// Override the directory where evidence files are written.
    pub fn with_evidence_root(mut self, path: impl Into<PathBuf>) -> Self {
        self.evidence_root = path.into();
        self
    }

    /// Attach a ledger so outcomes are persisted for replay.
    pub fn with_ledger(mut self, ledger: ValidatorLedger) -> Self {
        self.ledger = Some(ledger);
        self
    }

    /// Override the command policy (defaults to [`SafePolicy`]).
    pub fn with_policy(mut self, policy: Arc<dyn CommandPolicy>) -> Self {
        self.policy = policy;
        self
    }

    /// Run a batch of validators and return typed outcomes in the same order.
    pub async fn run_all(
        &self,
        invocation: &ValidatorInvocation,
        validators: &[Validator],
    ) -> Vec<ValidatorOutcome> {
        self.run_all_with_seeded_env(invocation, validators, &[])
            .await
    }

    /// Run validators, pre-seeding the given env vars on each spawned command
    /// validator child. Intended for tests that prove
    /// [`BLOCKED_ENV_VARS`] sanitization strips vars even if they were set
    /// explicitly on the `Command`. Not wired into production code paths.
    pub async fn run_all_with_seeded_env(
        &self,
        invocation: &ValidatorInvocation,
        validators: &[Validator],
        seeded_env: &[(&str, &str)],
    ) -> Vec<ValidatorOutcome> {
        let _ = std::fs::create_dir_all(&self.evidence_root);
        let mut outcomes = Vec::with_capacity(validators.len());
        for validator in validators {
            let started_at = Utc::now();
            let started = Instant::now();
            let kind_label = validator_kind_label(&validator.spec);
            let outcome = match &validator.spec {
                ValidatorSpec::Command { cmd, args } => {
                    self.run_command(
                        invocation, validator, cmd, args, started_at, started, seeded_env,
                    )
                    .await
                }
                ValidatorSpec::ToolCall { tool, args } => {
                    self.run_tool_call(invocation, validator, tool, args, started_at, started)
                        .await
                }
                ValidatorSpec::FileExists { path, min_bytes } => self
                    .run_file_exists(invocation, validator, path, *min_bytes, started_at, started),
            };

            record_counter(&outcome, kind_label);
            if let Some(ref ledger) = self.ledger {
                if let Err(err) = ledger.append(&outcome) {
                    warn!(
                        validator = %outcome.validator_id,
                        error = %err,
                        "failed to persist validator outcome"
                    );
                }
            }
            outcomes.push(outcome);
        }
        outcomes
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_command(
        &self,
        invocation: &ValidatorInvocation,
        validator: &Validator,
        cmd: &str,
        args: &[String],
        started_at: DateTime<Utc>,
        started: Instant,
        seeded_env: &[(&str, &str)],
    ) -> ValidatorOutcome {
        let timeout_ms = validator.timeout_ms.unwrap_or(DEFAULT_COMMAND_TIMEOUT_MS);
        let timeout_duration = Duration::from_millis(timeout_ms);

        // Shell-safety layer: SafePolicy denies the known-dangerous patterns.
        let command_string = build_command_string(cmd, args);
        let decision = self
            .policy
            .check(&command_string, &invocation.workspace_root);
        match decision {
            Decision::Allow => {}
            Decision::Deny | Decision::Ask => {
                return error_outcome(
                    invocation,
                    validator,
                    started_at,
                    started,
                    format!("command validator denied by safety policy: {command_string}"),
                );
            }
        }

        let mut command = Command::new(cmd);
        command
            .args(args)
            .current_dir(&invocation.workspace_root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());
        #[cfg(unix)]
        {
            // Put the child in its own process group so we can SIGTERM the
            // whole tree on timeout.
            command.process_group(0);
        }
        // Seeded env first (test hook); sanitization strips blocked ones.
        for (name, value) in seeded_env {
            command.env(*name, *value);
        }
        sanitize_command_env(&mut command, &EnvAllowlist::empty());

        let child = match command.spawn() {
            Ok(child) => child,
            Err(err) => {
                return error_outcome(
                    invocation,
                    validator,
                    started_at,
                    started,
                    format!("failed to spawn command validator: {err}"),
                );
            }
        };

        let child_pid = child.id();

        match timeout(timeout_duration, child.wait_with_output()).await {
            Ok(Ok(output)) => {
                let duration_ms = started.elapsed().as_millis() as u64;
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let exit_code = output.status.code();
                let evidence_path = self
                    .write_evidence(&validator.id, invocation, &stdout, &stderr, exit_code)
                    .await;
                let status = if output.status.success() {
                    ValidatorStatus::Pass
                } else {
                    ValidatorStatus::Fail
                };
                let reason = if output.status.success() {
                    format!(
                        "command validator succeeded (exit {})",
                        exit_code.unwrap_or(0)
                    )
                } else {
                    format!(
                        "command validator failed (exit {})",
                        exit_code.unwrap_or(-1)
                    )
                };
                let stderr_tail = stderr_tail(&stderr);
                ValidatorOutcome {
                    schema_version: VALIDATOR_RESULT_SCHEMA_VERSION,
                    validator_id: validator.id.clone(),
                    phase: invocation.phase,
                    kind: validator_kind_label(&validator.spec).to_string(),
                    repo_label: invocation.repo_label.clone(),
                    required: validator.required,
                    status,
                    reason,
                    duration_ms,
                    evidence_path,
                    stderr: stderr_tail,
                    started_at,
                }
            }
            Ok(Err(err)) => error_outcome(
                invocation,
                validator,
                started_at,
                started,
                format!("command validator wait failed: {err}"),
            ),
            Err(_) => {
                let duration_ms = started.elapsed().as_millis() as u64;
                if let Some(pid) = child_pid {
                    kill_child_process(pid).await;
                }
                ValidatorOutcome {
                    schema_version: VALIDATOR_RESULT_SCHEMA_VERSION,
                    validator_id: validator.id.clone(),
                    phase: invocation.phase,
                    kind: validator_kind_label(&validator.spec).to_string(),
                    repo_label: invocation.repo_label.clone(),
                    required: validator.required,
                    status: ValidatorStatus::Timeout,
                    reason: format!("command validator timed out after {timeout_ms}ms"),
                    duration_ms,
                    evidence_path: None,
                    stderr: None,
                    started_at,
                }
            }
        }
    }

    async fn run_tool_call(
        &self,
        invocation: &ValidatorInvocation,
        validator: &Validator,
        tool: &str,
        args: &serde_json::Value,
        started_at: DateTime<Utc>,
        started: Instant,
    ) -> ValidatorOutcome {
        let timeout_ms = validator.timeout_ms.unwrap_or(DEFAULT_COMMAND_TIMEOUT_MS);
        let timeout_duration = Duration::from_millis(timeout_ms);

        let dispatcher = self.dispatcher.clone();
        let tool_name = tool.to_string();
        let args_value = args.clone();
        let future = async move { dispatcher.dispatch(&tool_name, &args_value).await };

        match timeout(timeout_duration, future).await {
            Ok(Ok(result)) => {
                let duration_ms = started.elapsed().as_millis() as u64;
                let status = if result.success {
                    ValidatorStatus::Pass
                } else {
                    ValidatorStatus::Fail
                };
                let reason = if result.success {
                    format!("tool validator '{tool}' succeeded")
                } else {
                    result.output.clone()
                };
                let evidence_path = self
                    .write_evidence(&validator.id, invocation, &result.output, "", None)
                    .await;
                ValidatorOutcome {
                    schema_version: VALIDATOR_RESULT_SCHEMA_VERSION,
                    validator_id: validator.id.clone(),
                    phase: invocation.phase,
                    kind: validator_kind_label(&validator.spec).to_string(),
                    repo_label: invocation.repo_label.clone(),
                    required: validator.required,
                    status,
                    reason,
                    duration_ms,
                    evidence_path,
                    stderr: None,
                    started_at,
                }
            }
            Ok(Err(err)) => error_outcome(
                invocation,
                validator,
                started_at,
                started,
                format!("tool validator '{tool}' failed: {err}"),
            ),
            Err(_) => {
                let duration_ms = started.elapsed().as_millis() as u64;
                ValidatorOutcome {
                    schema_version: VALIDATOR_RESULT_SCHEMA_VERSION,
                    validator_id: validator.id.clone(),
                    phase: invocation.phase,
                    kind: validator_kind_label(&validator.spec).to_string(),
                    repo_label: invocation.repo_label.clone(),
                    required: validator.required,
                    status: ValidatorStatus::Timeout,
                    reason: format!("tool validator '{tool}' timed out after {timeout_ms}ms"),
                    duration_ms,
                    evidence_path: None,
                    stderr: None,
                    started_at,
                }
            }
        }
    }

    fn run_file_exists(
        &self,
        invocation: &ValidatorInvocation,
        validator: &Validator,
        path: &str,
        min_bytes: Option<u64>,
        started_at: DateTime<Utc>,
        started: Instant,
    ) -> ValidatorOutcome {
        let target = if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            invocation.workspace_root.join(path)
        };
        let duration_ms = started.elapsed().as_millis() as u64;
        let (status, reason) = match std::fs::metadata(&target) {
            Ok(meta) if meta.is_file() => {
                if let Some(min) = min_bytes {
                    if meta.len() < min {
                        (
                            ValidatorStatus::Fail,
                            format!(
                                "{} is {} bytes, min_bytes is {}",
                                target.display(),
                                meta.len(),
                                min
                            ),
                        )
                    } else {
                        (
                            ValidatorStatus::Pass,
                            format!(
                                "{} exists ({} bytes, min {})",
                                target.display(),
                                meta.len(),
                                min
                            ),
                        )
                    }
                } else {
                    (
                        ValidatorStatus::Pass,
                        format!("{} exists ({} bytes)", target.display(), meta.len()),
                    )
                }
            }
            Ok(_) => (
                ValidatorStatus::Fail,
                format!("{} is not a regular file", target.display()),
            ),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => (
                ValidatorStatus::Fail,
                format!("{} does not exist", target.display()),
            ),
            Err(err) => (
                ValidatorStatus::Error,
                format!("stat {} failed: {err}", target.display()),
            ),
        };

        ValidatorOutcome {
            schema_version: VALIDATOR_RESULT_SCHEMA_VERSION,
            validator_id: validator.id.clone(),
            phase: invocation.phase,
            kind: validator_kind_label(&validator.spec).to_string(),
            repo_label: invocation.repo_label.clone(),
            required: validator.required,
            status,
            reason,
            duration_ms,
            evidence_path: None,
            stderr: None,
            started_at,
        }
    }

    async fn write_evidence(
        &self,
        validator_id: &str,
        invocation: &ValidatorInvocation,
        stdout: &str,
        stderr: &str,
        exit_code: Option<i32>,
    ) -> Option<PathBuf> {
        if let Err(err) = tokio::fs::create_dir_all(&self.evidence_root).await {
            warn!(
                error = %err,
                dir = %self.evidence_root.display(),
                "failed to create validator evidence dir"
            );
            return None;
        }

        let stamp = Utc::now().format("%Y%m%dT%H%M%S%3f").to_string();
        let slug = slug_for_path(&invocation.repo_label);
        let filename = format!(
            "{slug}-{phase}-{id}-{stamp}.txt",
            phase = invocation.phase.as_str(),
            id = sanitize_filename_component(validator_id),
        );
        let path = self.evidence_root.join(filename);

        let mut buffer = String::new();
        buffer.push_str(&format!("validator_id={}\n", validator_id));
        buffer.push_str(&format!("phase={}\n", invocation.phase.as_str()));
        buffer.push_str(&format!("repo_label={}\n", invocation.repo_label));
        if let Some(code) = exit_code {
            buffer.push_str(&format!("exit_code={}\n", code));
        }
        buffer.push_str("---stdout---\n");
        buffer.push_str(&truncate_tail(stdout, MAX_EVIDENCE_BYTES / 2));
        buffer.push_str("\n---stderr---\n");
        buffer.push_str(&truncate_tail(stderr, MAX_EVIDENCE_BYTES / 2));

        match tokio::fs::File::create(&path).await {
            Ok(mut file) => {
                if let Err(err) = file.write_all(buffer.as_bytes()).await {
                    warn!(
                        error = %err,
                        path = %path.display(),
                        "failed to write validator evidence"
                    );
                    return None;
                }
                if let Err(err) = file.flush().await {
                    warn!(
                        error = %err,
                        path = %path.display(),
                        "failed to flush validator evidence"
                    );
                }
                Some(path)
            }
            Err(err) => {
                warn!(
                    error = %err,
                    path = %path.display(),
                    "failed to create validator evidence file"
                );
                None
            }
        }
    }
}

/// Build a representation of the command for the safety-policy check. This is
/// not forwarded to a shell — we only use it to run the denylist matcher.
fn build_command_string(cmd: &str, args: &[String]) -> String {
    let mut s = String::with_capacity(cmd.len() + args.iter().map(|a| a.len() + 1).sum::<usize>());
    s.push_str(cmd);
    for arg in args {
        s.push(' ');
        s.push_str(arg);
    }
    s
}

fn error_outcome(
    invocation: &ValidatorInvocation,
    validator: &Validator,
    started_at: DateTime<Utc>,
    started: Instant,
    reason: String,
) -> ValidatorOutcome {
    ValidatorOutcome {
        schema_version: VALIDATOR_RESULT_SCHEMA_VERSION,
        validator_id: validator.id.clone(),
        phase: invocation.phase,
        kind: validator_kind_label(&validator.spec).to_string(),
        repo_label: invocation.repo_label.clone(),
        required: validator.required,
        status: ValidatorStatus::Error,
        reason,
        duration_ms: started.elapsed().as_millis() as u64,
        evidence_path: None,
        stderr: None,
        started_at,
    }
}

fn validator_kind_label(spec: &ValidatorSpec) -> &'static str {
    match spec {
        ValidatorSpec::Command { .. } => "command",
        ValidatorSpec::ToolCall { .. } => "tool_call",
        ValidatorSpec::FileExists { .. } => "file_exists",
    }
}

fn stderr_tail(stderr: &str) -> Option<String> {
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(truncate_tail(trimmed, 4096))
}

fn truncate_tail(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    // Preserve the tail — most useful for diagnosing failures.
    let start = text.len() - max_bytes;
    let mut boundary = start;
    while boundary < text.len() && !text.is_char_boundary(boundary) {
        boundary += 1;
    }
    format!("...[truncated]\n{}", &text[boundary..])
}

fn slug_for_path(label: &str) -> String {
    label
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect()
}

fn sanitize_filename_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn record_counter(outcome: &ValidatorOutcome, kind_label: &'static str) {
    counter!(
        "octos_workspace_validator_total",
        "status" => outcome.status.as_str().to_string(),
        "phase" => outcome.phase.as_str().to_string(),
        "kind" => kind_label.to_string(),
        "required" => outcome.required.to_string(),
    )
    .increment(1);

    if outcome.required && outcome.status != ValidatorStatus::Pass {
        counter!("octos_workspace_validator_required_failed_total").increment(1);
    } else if !outcome.required && outcome.status != ValidatorStatus::Pass {
        counter!("octos_workspace_validator_optional_warning_total").increment(1);
    }
}

/// Kill a child process (and process group on Unix) cleanly. Used by the
/// command validator timeout handler.
async fn kill_child_process(pid: u32) {
    debug!(pid, "killing validator child on timeout");

    #[cfg(unix)]
    {
        use std::process::Command as StdCommand;
        let _ = StdCommand::new("kill")
            .args(["-15", &format!("-{pid}")])
            .status();
        let _ = StdCommand::new("kill")
            .args(["-15", &pid.to_string()])
            .status();
        tokio::time::sleep(KILL_GRACE_PERIOD).await;

        let still_alive = StdCommand::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .is_ok_and(|status| status.success());
        if still_alive {
            let _ = StdCommand::new("kill")
                .args(["-9", &format!("-{pid}")])
                .status();
            let _ = StdCommand::new("kill")
                .args(["-9", &pid.to_string()])
                .status();
        }
    }

    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .status();
    }
}

/// Convenience: run validators for a workspace contract inspection pass.
///
/// Consumers that already hold a policy + workspace root use this helper to
/// walk the typed validator list and collect outcomes.
pub async fn run_workspace_validators(
    runner: &ValidatorRunner,
    invocation: &ValidatorInvocation,
    validators: &[Validator],
    phase_filter: Option<ValidatorPhase>,
) -> Vec<ValidatorOutcome> {
    let filtered: Vec<Validator> = if let Some(phase) = phase_filter {
        validators
            .iter()
            .filter(|v| ValidatorPhase::from(v.phase) == phase)
            .cloned()
            .collect()
    } else {
        validators.to_vec()
    };
    runner.run_all(invocation, &filtered).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validator_kind_label_matches_spec() {
        assert_eq!(
            validator_kind_label(&ValidatorSpec::Command {
                cmd: "x".into(),
                args: Vec::new()
            }),
            "command"
        );
        assert_eq!(
            validator_kind_label(&ValidatorSpec::ToolCall {
                tool: "x".into(),
                args: serde_json::Value::Null
            }),
            "tool_call"
        );
        assert_eq!(
            validator_kind_label(&ValidatorSpec::FileExists {
                path: "x".into(),
                min_bytes: None
            }),
            "file_exists"
        );
    }

    #[test]
    fn truncate_tail_preserves_tail_on_overflow() {
        let input = "a".repeat(128);
        let out = truncate_tail(&input, 16);
        assert!(out.starts_with("...[truncated]\n"));
        assert!(out.ends_with("aaaaaaaaaaaaaaaa"));
    }

    #[test]
    fn schema_version_is_pinned() {
        assert_eq!(VALIDATOR_RESULT_SCHEMA_VERSION, 1);
    }

    #[test]
    fn required_gate_passes_only_on_pass() {
        let mut outcome = ValidatorOutcome {
            schema_version: VALIDATOR_RESULT_SCHEMA_VERSION,
            validator_id: "x".into(),
            phase: ValidatorPhase::Completion,
            kind: "command".into(),
            repo_label: "slides/x".into(),
            required: true,
            status: ValidatorStatus::Pass,
            reason: String::new(),
            duration_ms: 0,
            evidence_path: None,
            stderr: None,
            started_at: Utc::now(),
        };
        assert!(outcome.required_gate_passed());
        outcome.status = ValidatorStatus::Fail;
        assert!(!outcome.required_gate_passed());
        outcome.status = ValidatorStatus::Timeout;
        assert!(!outcome.required_gate_passed());
        outcome.status = ValidatorStatus::Error;
        assert!(!outcome.required_gate_passed());

        outcome.required = false;
        outcome.status = ValidatorStatus::Fail;
        assert!(outcome.required_gate_passed());
    }
}
