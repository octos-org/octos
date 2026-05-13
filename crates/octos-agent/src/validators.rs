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
use crate::workspace_policy::{MagicByteKind, Validator, ValidatorPhaseKind, ValidatorSpec};

/// Current schema version for [`ValidatorOutcome`] persistence.
pub const VALIDATOR_RESULT_SCHEMA_VERSION: u32 = 1;

const EVIDENCE_SUBDIR: &str = ".octos/validator-evidence";
const DEFAULT_COMMAND_TIMEOUT_MS: u64 = 30_000;
/// Default timeout for HTTP-probe validators when [`Validator::timeout_ms`]
/// is absent. Picked so a stale local API surface fails fast rather than
/// stalling the whole contract gate.
const DEFAULT_HTTP_PROBE_TIMEOUT_MS: u64 = 5_000;
const MAX_EVIDENCE_BYTES: usize = 512 * 1024;
const KILL_GRACE_PERIOD: Duration = Duration::from_millis(300);

/// Default ominix-api URL when the `OMINIX_API_URL` env override is absent.
const DEFAULT_OMINIX_API_URL: &str = "http://127.0.0.1:8081";

/// Test-only override for the ominix-api base URL.
///
/// Production reads the URL from the `OMINIX_API_URL` env var (or falls
/// back to [`DEFAULT_OMINIX_API_URL`]). Tests cannot safely mutate env vars
/// in 2024 edition under `deny(unsafe_code)`, so they install the address
/// of an in-test HTTP server here instead.
#[cfg(test)]
static TEST_OMINIX_URL_OVERRIDE: std::sync::OnceLock<std::sync::Mutex<Option<String>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
fn test_ominix_url_override() -> &'static std::sync::Mutex<Option<String>> {
    TEST_OMINIX_URL_OVERRIDE.get_or_init(|| std::sync::Mutex::new(None))
}

fn ominix_api_base_url() -> String {
    #[cfg(test)]
    {
        if let Ok(guard) = test_ominix_url_override().lock() {
            if let Some(ref url) = *guard {
                return url.clone();
            }
        }
    }
    std::env::var("OMINIX_API_URL").unwrap_or_else(|_| DEFAULT_OMINIX_API_URL.to_string())
}

/// Sample value, on a normalized -1.0..1.0 audio axis, above which a sample is
/// considered "non-silent". Matches the existing `mofa-podcast` skill's
/// non-silent heuristic so the validator and the skill agree on what counts
/// as silence.
const NON_SILENT_SAMPLE_FLOOR: f32 = 0.01;

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
///
/// `input_args` carries the originating spawn task's input JSON when this
/// invocation is run as part of a spawn-task contract gate. Domain validators
/// (`HttpProbe`, `OminixVoiceExists`) reference these args via
/// `${args.<key>}` template interpolation so they can assert e.g. "the
/// requested voice name is registered with ominix-api".
#[derive(Clone, Debug)]
pub struct ValidatorInvocation {
    pub phase: ValidatorPhase,
    pub workspace_root: PathBuf,
    pub repo_label: String,
    /// Optional input args from the originating spawn task. Used by
    /// `${args.<key>}` interpolation; absent for non-spawn contexts (e.g.
    /// turn-end validators that don't reference task inputs).
    pub input_args: Option<serde_json::Value>,
}

impl ValidatorInvocation {
    /// Build a `ValidatorInvocation` for a context that does not carry spawn
    /// task input args (e.g. turn-end validators, free-standing test setups).
    pub fn new(phase: ValidatorPhase, workspace_root: PathBuf, repo_label: String) -> Self {
        Self {
            phase,
            workspace_root,
            repo_label,
            input_args: None,
        }
    }

    /// Attach spawn task input args for `${args.<key>}` template
    /// interpolation by domain validators.
    pub fn with_input_args(mut self, args: serde_json::Value) -> Self {
        self.input_args = Some(args);
        self
    }
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
                ValidatorSpec::HttpProbe {
                    url_template,
                    expected_status,
                    expected_contains,
                } => {
                    self.run_http_probe(
                        invocation,
                        validator,
                        url_template,
                        *expected_status,
                        expected_contains.as_deref(),
                        started_at,
                        started,
                    )
                    .await
                }
                ValidatorSpec::OminixVoiceExists { name_arg } => {
                    self.run_ominix_voice_exists(
                        invocation, validator, name_arg, started_at, started,
                    )
                    .await
                }
                ValidatorSpec::AudioNonSilent { glob, min_ratio } => self.run_audio_non_silent(
                    invocation, validator, glob, *min_ratio, started_at, started,
                ),
                ValidatorSpec::MagicBytes { glob, format } => {
                    self.run_magic_bytes(invocation, validator, glob, *format, started_at, started)
                }
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

    /// Run an HTTP-probe validator.
    ///
    /// Interpolates `${args.<key>}` against the spawn task's input args, then
    /// performs a GET against the resulting URL and asserts the status code
    /// (and optionally a substring of the body) matches the spec.
    #[allow(clippy::too_many_arguments)]
    async fn run_http_probe(
        &self,
        invocation: &ValidatorInvocation,
        validator: &Validator,
        url_template: &str,
        expected_status: u16,
        expected_contains: Option<&str>,
        started_at: DateTime<Utc>,
        started: Instant,
    ) -> ValidatorOutcome {
        let timeout_ms = validator
            .timeout_ms
            .unwrap_or(DEFAULT_HTTP_PROBE_TIMEOUT_MS);
        let url = match interpolate_args(url_template, invocation.input_args.as_ref()) {
            Ok(url) => url,
            Err(reason) => {
                return error_outcome(invocation, validator, started_at, started, reason);
            }
        };

        match probe_http(&url, timeout_ms, expected_status, expected_contains).await {
            Ok(reason) => self.make_outcome(
                invocation,
                validator,
                ValidatorStatus::Pass,
                reason,
                started_at,
                started,
            ),
            Err(HttpProbeFailure::Timeout) => self.make_outcome(
                invocation,
                validator,
                ValidatorStatus::Timeout,
                format!("http probe timed out after {timeout_ms}ms: {url}"),
                started_at,
                started,
            ),
            Err(HttpProbeFailure::Fail(reason)) => self.make_outcome(
                invocation,
                validator,
                ValidatorStatus::Fail,
                reason,
                started_at,
                started,
            ),
            Err(HttpProbeFailure::Error(reason)) => self.make_outcome(
                invocation,
                validator,
                ValidatorStatus::Error,
                reason,
                started_at,
                started,
            ),
        }
    }

    /// Run an OminixVoiceExists validator.
    ///
    /// Calls `GET ${OMINIX_API_URL:-http://127.0.0.1:8081}/v1/voices` and
    /// asserts the JSON body's `voices[].name` array contains the voice
    /// name resolved from the spawn task's input args.
    async fn run_ominix_voice_exists(
        &self,
        invocation: &ValidatorInvocation,
        validator: &Validator,
        name_arg: &str,
        started_at: DateTime<Utc>,
        started: Instant,
    ) -> ValidatorOutcome {
        let timeout_ms = validator
            .timeout_ms
            .unwrap_or(DEFAULT_HTTP_PROBE_TIMEOUT_MS);
        let base = ominix_api_base_url();
        let url = format!("{}/v1/voices", base.trim_end_matches('/'));
        let voice_name = match input_arg(invocation.input_args.as_ref(), name_arg) {
            Some(value) => value,
            None => {
                return self.make_outcome(
                    invocation,
                    validator,
                    ValidatorStatus::Error,
                    format!("ominix_voice_exists: input args missing key '{name_arg}'"),
                    started_at,
                    started,
                );
            }
        };
        match fetch_ominix_voices(&url, timeout_ms).await {
            Ok(voices) => {
                if voices.iter().any(|v| v == &voice_name) {
                    self.make_outcome(
                        invocation,
                        validator,
                        ValidatorStatus::Pass,
                        format!(
                            "ominix voice '{voice_name}' is registered (out of {} total)",
                            voices.len()
                        ),
                        started_at,
                        started,
                    )
                } else {
                    let preview = if voices.is_empty() {
                        "<none>".to_string()
                    } else {
                        voices.join(", ")
                    };
                    self.make_outcome(
                        invocation,
                        validator,
                        ValidatorStatus::Fail,
                        format!(
                            "ominix voice '{voice_name}' is not registered. Available voices: {preview}"
                        ),
                        started_at,
                        started,
                    )
                }
            }
            Err(HttpProbeFailure::Timeout) => self.make_outcome(
                invocation,
                validator,
                ValidatorStatus::Timeout,
                format!("ominix /v1/voices timed out after {timeout_ms}ms: {url}"),
                started_at,
                started,
            ),
            Err(HttpProbeFailure::Fail(reason)) | Err(HttpProbeFailure::Error(reason)) => self
                .make_outcome(
                    invocation,
                    validator,
                    ValidatorStatus::Error,
                    reason,
                    started_at,
                    started,
                ),
        }
    }

    /// Run an AudioNonSilent validator.
    fn run_audio_non_silent(
        &self,
        invocation: &ValidatorInvocation,
        validator: &Validator,
        pattern: &str,
        min_ratio: f32,
        started_at: DateTime<Utc>,
        started: Instant,
    ) -> ValidatorOutcome {
        let matches = match glob_files(&invocation.workspace_root, pattern) {
            Ok(matches) => matches,
            Err(reason) => {
                return self.make_outcome(
                    invocation,
                    validator,
                    ValidatorStatus::Error,
                    reason,
                    started_at,
                    started,
                );
            }
        };
        if matches.is_empty() {
            return self.make_outcome(
                invocation,
                validator,
                ValidatorStatus::Fail,
                format!("audio_non_silent: no files matched '{pattern}'"),
                started_at,
                started,
            );
        }

        let mut passed_any = false;
        let mut failures = Vec::new();
        for path in &matches {
            match decode_non_silent_ratio(path) {
                Ok(ratio) if ratio >= min_ratio => {
                    passed_any = true;
                    break;
                }
                Ok(ratio) => failures.push(format!(
                    "{}: non_silent_ratio={ratio:.3} < min_ratio={min_ratio:.3}",
                    path.display()
                )),
                Err(reason) => failures.push(format!("{}: {reason}", path.display())),
            }
        }

        if passed_any {
            self.make_outcome(
                invocation,
                validator,
                ValidatorStatus::Pass,
                format!("audio_non_silent: at least one file met min_ratio={min_ratio:.3}"),
                started_at,
                started,
            )
        } else {
            self.make_outcome(
                invocation,
                validator,
                ValidatorStatus::Fail,
                format!("audio_non_silent failed: {}", failures.join("; ")),
                started_at,
                started,
            )
        }
    }

    /// Run a MagicBytes validator.
    fn run_magic_bytes(
        &self,
        invocation: &ValidatorInvocation,
        validator: &Validator,
        pattern: &str,
        kind: MagicByteKind,
        started_at: DateTime<Utc>,
        started: Instant,
    ) -> ValidatorOutcome {
        let matches = match glob_files(&invocation.workspace_root, pattern) {
            Ok(matches) => matches,
            Err(reason) => {
                return self.make_outcome(
                    invocation,
                    validator,
                    ValidatorStatus::Error,
                    reason,
                    started_at,
                    started,
                );
            }
        };
        if matches.is_empty() {
            return self.make_outcome(
                invocation,
                validator,
                ValidatorStatus::Fail,
                format!("magic_bytes: no files matched '{pattern}'"),
                started_at,
                started,
            );
        }

        let mut failures = Vec::new();
        for path in &matches {
            match read_magic_prefix(path) {
                Ok(prefix) => {
                    if !kind.matches(&prefix) {
                        failures.push(format!(
                            "{}: header does not match {} magic bytes",
                            path.display(),
                            kind.as_str()
                        ));
                    }
                }
                Err(reason) => {
                    failures.push(format!("{}: read failed: {reason}", path.display()));
                }
            }
        }
        if failures.is_empty() {
            self.make_outcome(
                invocation,
                validator,
                ValidatorStatus::Pass,
                format!(
                    "magic_bytes: all {} match(es) carry {} signature",
                    matches.len(),
                    kind.as_str()
                ),
                started_at,
                started,
            )
        } else {
            self.make_outcome(
                invocation,
                validator,
                ValidatorStatus::Fail,
                format!("magic_bytes failed: {}", failures.join("; ")),
                started_at,
                started,
            )
        }
    }

    fn make_outcome(
        &self,
        invocation: &ValidatorInvocation,
        validator: &Validator,
        status: ValidatorStatus,
        reason: String,
        started_at: DateTime<Utc>,
        started: Instant,
    ) -> ValidatorOutcome {
        ValidatorOutcome {
            schema_version: VALIDATOR_RESULT_SCHEMA_VERSION,
            validator_id: validator.id.clone(),
            phase: invocation.phase,
            kind: validator_kind_label(&validator.spec).to_string(),
            repo_label: invocation.repo_label.clone(),
            required: validator.required,
            status,
            reason,
            duration_ms: started.elapsed().as_millis() as u64,
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

/// Internal failure category for HTTP-probe validators.
///
/// Threaded back out of [`probe_http`] so the runner can map each category
/// onto the correct typed [`ValidatorStatus`] (`Timeout`, `Fail`, `Error`).
enum HttpProbeFailure {
    Timeout,
    Fail(String),
    Error(String),
}

/// Substitute `${args.<key>}` references in `template` against `input_args`.
///
/// `<key>` is a dotted JSON path against the input args object. A missing key
/// or a non-string/number value is a hard error so the validator surfaces an
/// `Error` outcome rather than silently degrading the URL.
///
/// Substituted values are percent-encoded against
/// [`URL_PATH_QUERY_RESERVED`] before being spliced into the template so an
/// LLM- or user-controlled arg value cannot break out of the path/query
/// segment it was placed into (e.g. inject a different host or query
/// parameter). Templates are treated as URL fragments, not opaque strings.
fn interpolate_args(
    template: &str,
    input_args: Option<&serde_json::Value>,
) -> Result<String, String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("${args.") {
        out.push_str(&rest[..start]);
        let after = &rest[start + "${args.".len()..];
        let end = after
            .find('}')
            .ok_or_else(|| format!("unterminated ${{args.}} reference in template: {template}"))?;
        let key = &after[..end];
        let value = input_arg(input_args, key).ok_or_else(|| {
            format!("input arg '{key}' not found while interpolating template: {template}")
        })?;
        out.push_str(&percent_encode_url_segment(&value));
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Percent-encode bytes that have reserved meaning in URL path/query segments
/// so an LLM-controlled arg value cannot break out of the segment it was
/// placed into. Conservative: encodes everything outside the unreserved set
/// defined in RFC 3986 plus the `~` allowed-in-unreserved character.
fn percent_encode_url_segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        let unreserved = byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'_' | b'.' | b'~');
        if unreserved {
            out.push(*byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Fetch a single argument value from a spawn task's input args by dotted key.
fn input_arg(input_args: Option<&serde_json::Value>, key: &str) -> Option<String> {
    let mut value = input_args?;
    for part in key.split('.') {
        if part.is_empty() {
            return None;
        }
        value = value.get(part)?;
    }
    match value {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Perform an HTTP GET probe and assert the response shape.
async fn probe_http(
    url: &str,
    timeout_ms: u64,
    expected_status: u16,
    expected_contains: Option<&str>,
) -> Result<String, HttpProbeFailure> {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            return Err(HttpProbeFailure::Error(format!(
                "build http client failed: {err}"
            )));
        }
    };
    let response = match client.get(url).send().await {
        Ok(response) => response,
        Err(err) if err.is_timeout() => return Err(HttpProbeFailure::Timeout),
        Err(err) => {
            return Err(HttpProbeFailure::Error(format!(
                "http probe request failed for {url}: {err}"
            )));
        }
    };
    let actual = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    if actual != expected_status {
        let preview = preview_body(&body);
        return Err(HttpProbeFailure::Fail(format!(
            "http probe got status {actual} (expected {expected_status}) at {url}; body preview: {preview}"
        )));
    }
    if let Some(needle) = expected_contains {
        if !body.contains(needle) {
            let preview = preview_body(&body);
            return Err(HttpProbeFailure::Fail(format!(
                "http probe body at {url} did not contain '{needle}'; body preview: {preview}"
            )));
        }
    }
    Ok(format!(
        "http probe {url} returned status {actual}{}",
        match expected_contains {
            Some(needle) => format!(" with substring '{needle}'"),
            None => String::new(),
        }
    ))
}

fn preview_body(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return "<empty>".to_string();
    }
    if trimmed.len() <= 200 {
        return trimmed.to_string();
    }
    let mut end = 200;
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &trimmed[..end])
}

/// Fetch ominix-api `/v1/voices` and extract the registered voice names.
async fn fetch_ominix_voices(url: &str, timeout_ms: u64) -> Result<Vec<String>, HttpProbeFailure> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .build()
        .map_err(|err| HttpProbeFailure::Error(format!("build http client failed: {err}")))?;
    let response = match client.get(url).send().await {
        Ok(response) => response,
        Err(err) if err.is_timeout() => return Err(HttpProbeFailure::Timeout),
        Err(err) => {
            return Err(HttpProbeFailure::Error(format!(
                "ominix /v1/voices fetch failed at {url}: {err}"
            )));
        }
    };
    if !response.status().is_success() {
        return Err(HttpProbeFailure::Error(format!(
            "ominix /v1/voices returned status {} at {url}",
            response.status().as_u16()
        )));
    }
    let body = response.text().await.unwrap_or_default();
    let parsed: serde_json::Value = serde_json::from_str(&body)
        .map_err(|err| HttpProbeFailure::Error(format!("ominix /v1/voices invalid JSON: {err}")))?;
    let voices = parsed
        .get("voices")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            HttpProbeFailure::Error(format!(
                "ominix /v1/voices response missing 'voices' array (body preview: {})",
                preview_body(&body)
            ))
        })?;
    let names: Vec<String> = voices
        .iter()
        .filter_map(|entry| {
            entry
                .get("name")
                .and_then(|name| name.as_str())
                .map(str::to_string)
        })
        .collect();
    Ok(names)
}

/// Resolve a glob pattern against `workspace_root` and return matching files
/// (skipping directories).
fn glob_files(workspace_root: &Path, pattern: &str) -> Result<Vec<PathBuf>, String> {
    let absolute_pattern = if Path::new(pattern).is_absolute() {
        PathBuf::from(pattern)
    } else {
        workspace_root.join(pattern)
    };
    let mut matches = Vec::new();
    for entry in glob::glob(&absolute_pattern.to_string_lossy())
        .map_err(|err| format!("invalid glob '{pattern}': {err}"))?
    {
        let path = entry.map_err(|err| format!("glob '{pattern}' failed: {err}"))?;
        if path.is_file() {
            matches.push(path);
        }
    }
    Ok(matches)
}

/// Read the first 32 bytes of a file for magic-byte sniffing.
fn read_magic_prefix(path: &Path) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(|err| format!("open failed: {err}"))?;
    let mut buf = [0u8; 32];
    let n = file
        .read(&mut buf)
        .map_err(|err| format!("read failed: {err}"))?;
    Ok(buf[..n].to_vec())
}

/// Decode `path` (WAV via [`hound`], or MP3 via the optional `audio_mp3`
/// feature) and return the ratio of non-silent samples to total samples.
fn decode_non_silent_ratio(path: &Path) -> Result<f32, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "wav" | "wave" => decode_non_silent_ratio_wav(path),
        "mp3" => decode_non_silent_ratio_mp3(path),
        other => Err(format!(
            "audio_non_silent: unsupported file extension '{other}' (supported: wav, mp3)"
        )),
    }
}

fn decode_non_silent_ratio_wav(path: &Path) -> Result<f32, String> {
    let mut reader =
        hound::WavReader::open(path).map_err(|err| format!("wav open failed: {err}"))?;
    let spec = reader.spec();
    let mut total: u64 = 0;
    let mut non_silent: u64 = 0;
    let denom = match spec.sample_format {
        hound::SampleFormat::Float => 1.0_f32,
        // PCM full-scale magnitude per bit depth: (2^(bits-1)) - 1. The
        // earlier coarse approximation (`i32::MAX` for both 24 and 32 bit)
        // mis-normalized 24-bit samples by a factor of 256, so a perfectly
        // loud 24-bit recording fell below the 0.01 non-silent floor.
        hound::SampleFormat::Int => match spec.bits_per_sample {
            8 => i8::MAX as f32,
            16 => i16::MAX as f32,
            24 => ((1u32 << 23) - 1) as f32,
            32 => i32::MAX as f32,
            other => {
                return Err(format!("unsupported wav bits_per_sample={other}"));
            }
        },
    };
    match spec.sample_format {
        hound::SampleFormat::Float => {
            for sample in reader.samples::<f32>() {
                let value = sample.map_err(|err| format!("wav decode failed: {err}"))?;
                total += 1;
                if value.abs() > NON_SILENT_SAMPLE_FLOOR {
                    non_silent += 1;
                }
            }
        }
        hound::SampleFormat::Int => {
            for sample in reader.samples::<i32>() {
                let value = sample.map_err(|err| format!("wav decode failed: {err}"))? as f32;
                total += 1;
                if (value / denom).abs() > NON_SILENT_SAMPLE_FLOOR {
                    non_silent += 1;
                }
            }
        }
    }
    if total == 0 {
        return Err("wav file has zero samples".to_string());
    }
    Ok(non_silent as f32 / total as f32)
}

#[cfg(feature = "audio_mp3")]
fn decode_non_silent_ratio_mp3(path: &Path) -> Result<f32, String> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::DecoderOptions;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let file = std::fs::File::open(path).map_err(|err| format!("mp3 open failed: {err}"))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    hint.with_extension("mp3");
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|err| format!("mp3 probe failed: {err}"))?;
    let mut format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| "mp3 file has no default track".to_string())?;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|err| format!("mp3 decoder init failed: {err}"))?;
    let track_id = track.id;
    let mut total: u64 = 0;
    let mut non_silent: u64 = 0;
    let mut sample_buf: Option<SampleBuffer<f32>> = None;
    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(symphonia::core::errors::Error::IoError(err))
                if err.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(symphonia::core::errors::Error::ResetRequired) => break,
            Err(err) => return Err(format!("mp3 read failed: {err}")),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(symphonia::core::errors::Error::IoError(_)) => break,
            Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
            Err(err) => return Err(format!("mp3 decode failed: {err}")),
        };
        if sample_buf.is_none() {
            let spec = *decoded.spec();
            sample_buf = Some(SampleBuffer::new(decoded.capacity() as u64, spec));
        }
        if let Some(ref mut buf) = sample_buf {
            buf.copy_interleaved_ref(decoded);
            for &sample in buf.samples() {
                total += 1;
                if sample.abs() > NON_SILENT_SAMPLE_FLOOR {
                    non_silent += 1;
                }
            }
        }
    }
    if total == 0 {
        return Err("mp3 file decoded zero samples".to_string());
    }
    Ok(non_silent as f32 / total as f32)
}

#[cfg(not(feature = "audio_mp3"))]
fn decode_non_silent_ratio_mp3(_path: &Path) -> Result<f32, String> {
    Err(
        "audio_non_silent for .mp3 requires the 'audio_mp3' feature; \
         enable it on octos-agent or use a .wav input"
            .to_string(),
    )
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
        ValidatorSpec::HttpProbe { .. } => "http_probe",
        ValidatorSpec::OminixVoiceExists { .. } => "ominix_voice_exists",
        ValidatorSpec::AudioNonSilent { .. } => "audio_non_silent",
        ValidatorSpec::MagicBytes { .. } => "magic_bytes",
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
        assert_eq!(
            validator_kind_label(&ValidatorSpec::HttpProbe {
                url_template: "http://x".into(),
                expected_status: 200,
                expected_contains: None,
            }),
            "http_probe"
        );
        assert_eq!(
            validator_kind_label(&ValidatorSpec::OminixVoiceExists {
                name_arg: "name".into()
            }),
            "ominix_voice_exists"
        );
        assert_eq!(
            validator_kind_label(&ValidatorSpec::AudioNonSilent {
                glob: "*.wav".into(),
                min_ratio: 0.3
            }),
            "audio_non_silent"
        );
        assert_eq!(
            validator_kind_label(&ValidatorSpec::MagicBytes {
                glob: "*.mp3".into(),
                format: crate::workspace_policy::MagicByteKind::Mp3,
            }),
            "magic_bytes"
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

    // ---------------------------------------------------------------------
    // Helpers for the domain-validator tests (HTTP probe, audio, magic bytes)
    // ---------------------------------------------------------------------

    use std::io::{Read, Write as IoWrite};
    use std::net::TcpListener;

    fn dummy_invocation(workspace_root: PathBuf) -> ValidatorInvocation {
        ValidatorInvocation::new(ValidatorPhase::Completion, workspace_root, "test".into())
    }

    fn validator_with_spec(id: &str, spec: ValidatorSpec) -> Validator {
        Validator {
            id: id.into(),
            required: true,
            timeout_ms: Some(2000),
            phase: ValidatorPhaseKind::Completion,
            spec,
        }
    }

    /// Tiny synchronous HTTP server scripted via `responses`. Spawns a thread,
    /// listens on `127.0.0.1:0`, replies to each accepted connection in order,
    /// and exits once `responses.len()` connections have been served. Returns
    /// the listener's bound `host:port` for the test to point validators at.
    fn spawn_test_http_server(responses: Vec<&'static str>) -> String {
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

    #[tokio::test]
    async fn http_probe_passes_on_expected_status_and_substring() {
        let response = "HTTP/1.1 200 OK\r\nContent-Length: 14\r\n\r\n{\"ok\":\"yangmi\"}";
        let addr = spawn_test_http_server(vec![response]);
        let url = format!("http://{addr}/voices/yangmi");
        let dir = tempfile::tempdir().unwrap();
        let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
        let validator = validator_with_spec(
            "probe_ok",
            ValidatorSpec::HttpProbe {
                url_template: url.clone(),
                expected_status: 200,
                expected_contains: Some("yangmi".into()),
            },
        );
        let outcomes = runner
            .run_all(&dummy_invocation(dir.path().to_path_buf()), &[validator])
            .await;
        assert_eq!(outcomes[0].status, ValidatorStatus::Pass, "{outcomes:?}");
    }

    #[tokio::test]
    async fn http_probe_fails_on_404_status() {
        let response = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
        let addr = spawn_test_http_server(vec![response]);
        let url = format!("http://{addr}/missing");
        let dir = tempfile::tempdir().unwrap();
        let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
        let validator = validator_with_spec(
            "probe_404",
            ValidatorSpec::HttpProbe {
                url_template: url,
                expected_status: 200,
                expected_contains: None,
            },
        );
        let outcomes = runner
            .run_all(&dummy_invocation(dir.path().to_path_buf()), &[validator])
            .await;
        assert_eq!(outcomes[0].status, ValidatorStatus::Fail);
        assert!(outcomes[0].reason.contains("got status 404"));
    }

    #[tokio::test]
    async fn http_probe_fails_when_body_missing_expected_substring() {
        let response = "HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nNOPE";
        let addr = spawn_test_http_server(vec![response]);
        let url = format!("http://{addr}/x");
        let dir = tempfile::tempdir().unwrap();
        let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
        let validator = validator_with_spec(
            "probe_no_substring",
            ValidatorSpec::HttpProbe {
                url_template: url,
                expected_status: 200,
                expected_contains: Some("yangmi".into()),
            },
        );
        let outcomes = runner
            .run_all(&dummy_invocation(dir.path().to_path_buf()), &[validator])
            .await;
        assert_eq!(outcomes[0].status, ValidatorStatus::Fail);
        assert!(outcomes[0].reason.contains("did not contain 'yangmi'"));
    }

    #[tokio::test]
    async fn http_probe_interpolates_args_into_url_template() {
        let response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK";
        let addr = spawn_test_http_server(vec![response]);
        let url_template = format!("http://{addr}/voices/${{args.name}}");
        let dir = tempfile::tempdir().unwrap();
        let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
        let invocation = ValidatorInvocation::new(
            ValidatorPhase::Completion,
            dir.path().to_path_buf(),
            "test".into(),
        )
        .with_input_args(serde_json::json!({"name": "yangmi"}));
        let validator = validator_with_spec(
            "probe_interp",
            ValidatorSpec::HttpProbe {
                url_template,
                expected_status: 200,
                expected_contains: None,
            },
        );
        let outcomes = runner.run_all(&invocation, &[validator]).await;
        assert_eq!(outcomes[0].status, ValidatorStatus::Pass, "{outcomes:?}");
        // The successful reason should reference the interpolated URL.
        assert!(
            outcomes[0].reason.contains("/voices/yangmi"),
            "missing interpolated value in: {}",
            outcomes[0].reason
        );
    }

    /// RAII guard that installs an ominix-api URL override in
    /// [`TEST_OMINIX_URL_OVERRIDE`] and clears it on drop.
    struct OminixUrlGuard;

    impl OminixUrlGuard {
        fn install(url: String) -> Self {
            *test_ominix_url_override().lock().unwrap() = Some(url);
            Self
        }
    }

    impl Drop for OminixUrlGuard {
        fn drop(&mut self) {
            *test_ominix_url_override().lock().unwrap() = None;
        }
    }

    /// Serialize ominix tests on the shared URL override slot. Using an
    /// async-aware `tokio::sync::Mutex` here so the guard can safely cross
    /// `.await` points (the test holds it across the in-test HTTP probe).
    fn ominix_test_lock() -> &'static tokio::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    #[tokio::test]
    async fn ominix_voice_exists_passes_when_name_in_voice_list() {
        let _serial = ominix_test_lock().lock().await;
        let body = "{\"voices\":[{\"name\":\"vivian\",\"aliases\":[]},{\"name\":\"serena\"}]}";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let leaked: &'static str = Box::leak(response.into_boxed_str());
        let addr = spawn_test_http_server(vec![leaked]);
        let _guard = OminixUrlGuard::install(format!("http://{addr}"));
        let dir = tempfile::tempdir().unwrap();
        let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
        let invocation = ValidatorInvocation::new(
            ValidatorPhase::Completion,
            dir.path().to_path_buf(),
            "test".into(),
        )
        .with_input_args(serde_json::json!({"name": "vivian"}));
        let validator = validator_with_spec(
            "voice_pass",
            ValidatorSpec::OminixVoiceExists {
                name_arg: "name".into(),
            },
        );
        let outcomes = runner.run_all(&invocation, &[validator]).await;
        assert_eq!(outcomes[0].status, ValidatorStatus::Pass, "{outcomes:?}");
    }

    #[tokio::test]
    async fn ominix_voice_exists_fails_with_available_list_on_missing_name() {
        let _serial = ominix_test_lock().lock().await;
        let body = "{\"voices\":[{\"name\":\"vivian\"},{\"name\":\"serena\"}]}";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let leaked: &'static str = Box::leak(response.into_boxed_str());
        let addr = spawn_test_http_server(vec![leaked]);
        let _guard = OminixUrlGuard::install(format!("http://{addr}"));
        let dir = tempfile::tempdir().unwrap();
        let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
        let invocation = ValidatorInvocation::new(
            ValidatorPhase::Completion,
            dir.path().to_path_buf(),
            "test".into(),
        )
        .with_input_args(serde_json::json!({"name": "yangmi"}));
        let validator = validator_with_spec(
            "voice_fail",
            ValidatorSpec::OminixVoiceExists {
                name_arg: "name".into(),
            },
        );
        let outcomes = runner.run_all(&invocation, &[validator]).await;
        assert_eq!(outcomes[0].status, ValidatorStatus::Fail);
        // Failure message must surface the available list so the LLM can
        // react in one round.
        assert!(
            outcomes[0].reason.contains("yangmi"),
            "missing requested name in reason: {}",
            outcomes[0].reason
        );
        assert!(
            outcomes[0].reason.contains("vivian") && outcomes[0].reason.contains("serena"),
            "missing available list in reason: {}",
            outcomes[0].reason
        );
    }

    /// Generate a WAV file at `path` filled with silence.
    fn write_silent_wav(path: &Path, samples: usize) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 8_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(path, spec).expect("create wav");
        for _ in 0..samples {
            writer.write_sample(0i16).expect("write sample");
        }
        writer.finalize().expect("finalize wav");
    }

    /// Generate a WAV sine wave at `path`. Loud enough that every sample is
    /// above [`NON_SILENT_SAMPLE_FLOOR`].
    fn write_sine_wav(path: &Path, samples: usize) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 8_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
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

    #[tokio::test]
    async fn audio_non_silent_fails_for_silent_wav() {
        let dir = tempfile::tempdir().unwrap();
        let audio_path = dir.path().join("silent.wav");
        write_silent_wav(&audio_path, 800);
        let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
        let validator = validator_with_spec(
            "silent_audio",
            ValidatorSpec::AudioNonSilent {
                glob: "*.wav".into(),
                min_ratio: 0.3,
            },
        );
        let outcomes = runner
            .run_all(&dummy_invocation(dir.path().to_path_buf()), &[validator])
            .await;
        assert_eq!(outcomes[0].status, ValidatorStatus::Fail, "{outcomes:?}");
        assert!(
            outcomes[0].reason.contains("non_silent_ratio"),
            "reason should expose ratio: {}",
            outcomes[0].reason
        );
    }

    #[tokio::test]
    async fn audio_non_silent_passes_for_sine_wav() {
        let dir = tempfile::tempdir().unwrap();
        let audio_path = dir.path().join("sine.wav");
        write_sine_wav(&audio_path, 800);
        let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
        let validator = validator_with_spec(
            "loud_audio",
            ValidatorSpec::AudioNonSilent {
                glob: "*.wav".into(),
                min_ratio: 0.3,
            },
        );
        let outcomes = runner
            .run_all(&dummy_invocation(dir.path().to_path_buf()), &[validator])
            .await;
        assert_eq!(outcomes[0].status, ValidatorStatus::Pass, "{outcomes:?}");
    }

    #[tokio::test]
    async fn magic_bytes_passes_for_valid_mp3_id3_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("song.mp3");
        let mut bytes = b"ID3".to_vec();
        bytes.extend(std::iter::repeat_n(0u8, 128));
        std::fs::write(&path, &bytes).unwrap();
        let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
        let validator = validator_with_spec(
            "mp3_ok",
            ValidatorSpec::MagicBytes {
                glob: "*.mp3".into(),
                format: crate::workspace_policy::MagicByteKind::Mp3,
            },
        );
        let outcomes = runner
            .run_all(&dummy_invocation(dir.path().to_path_buf()), &[validator])
            .await;
        assert_eq!(outcomes[0].status, ValidatorStatus::Pass, "{outcomes:?}");
    }

    #[tokio::test]
    async fn magic_bytes_fails_when_file_is_actually_gif() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not_mp3.mp3");
        std::fs::write(&path, b"GIF87a\0\0\0").unwrap();
        let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
        let validator = validator_with_spec(
            "mp3_bad",
            ValidatorSpec::MagicBytes {
                glob: "*.mp3".into(),
                format: crate::workspace_policy::MagicByteKind::Mp3,
            },
        );
        let outcomes = runner
            .run_all(&dummy_invocation(dir.path().to_path_buf()), &[validator])
            .await;
        assert_eq!(outcomes[0].status, ValidatorStatus::Fail, "{outcomes:?}");
        assert!(outcomes[0].reason.contains("does not match mp3"));
    }

    #[test]
    fn interpolate_args_substitutes_simple_key() {
        let args = serde_json::json!({"name": "yangmi"});
        let out = interpolate_args("http://x/${args.name}", Some(&args)).unwrap();
        assert_eq!(out, "http://x/yangmi");
    }

    #[test]
    fn interpolate_args_errors_when_key_missing() {
        let args = serde_json::json!({});
        let err = interpolate_args("http://x/${args.name}", Some(&args)).unwrap_err();
        assert!(err.contains("'name'"));
    }

    #[test]
    fn interpolate_args_percent_encodes_reserved_characters() {
        // An LLM-controlled value MUST NOT be able to break out of the URL
        // segment it lands in. `?`, `&`, `/`, `#` etc. must be percent-
        // encoded so the resulting URL has the literal value as a single
        // path segment, not a structural separator.
        let args = serde_json::json!({"name": "evil/../?inject=1"});
        let out = interpolate_args("http://x/${args.name}", Some(&args)).unwrap();
        // The interpolated segment should not contain raw `/`, `?`, or `=`.
        let interpolated = out.strip_prefix("http://x/").expect("prefix preserved");
        assert!(
            !interpolated.contains('/'),
            "raw `/` leaked: {interpolated}"
        );
        assert!(
            !interpolated.contains('?'),
            "raw `?` leaked: {interpolated}"
        );
        assert!(
            !interpolated.contains('='),
            "raw `=` leaked: {interpolated}"
        );
    }
}
