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
use crate::sandbox::Sandbox;
use crate::subprocess_env::{EnvAllowlist, sanitize_command_env};
use crate::tools::{ToolRegistry, ToolResult};
use crate::workspace_policy::{
    MagicByteKind, Validator, ValidatorFileSource, ValidatorPhaseKind, ValidatorSpec,
};

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

/// Default `required_tier` for legacy ledger records emitted before Wave-3a.
///
/// Sentinel value (`""`) — replaced with the tier derived from the legacy
/// `required` field by [`ValidatorOutcome::normalize_legacy_tier`] after
/// deserialize. We can't peek at sibling fields during a `serde(default = ...)`
/// callback, so the normalization happens explicitly on every read path.
fn default_required_tier() -> String {
    String::new()
}

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
///
/// `tool_output` carries the spawn task tool's `named_outputs` map (e.g.
/// `mofa_publish` emits `deploy_url`). Domain validators reference these via
/// `${output.<key>}` interpolation so they can probe the live URL the tool
/// just produced. Absent for non-spawn contexts and for tools that emit no
/// named outputs.
#[derive(Clone, Debug)]
pub struct ValidatorInvocation {
    pub phase: ValidatorPhase,
    pub workspace_root: PathBuf,
    pub repo_label: String,
    /// Optional input args from the originating spawn task. Used by
    /// `${args.<key>}` interpolation; absent for non-spawn contexts (e.g.
    /// turn-end validators that don't reference task inputs).
    pub input_args: Option<serde_json::Value>,
    /// Optional `named_outputs` map from the spawn task tool's stdout
    /// envelope. Used by `${output.<key>}` interpolation; absent when the
    /// tool emitted no named outputs (most legacy plugins).
    pub tool_output: Option<serde_json::Value>,
    /// Optional `files_to_send` list from the originating spawn_only tool's
    /// stdout envelope (the plugin protocol's authoritative list of files
    /// the skill just produced). Consumed by file-list-driven validators
    /// (`MagicBytes`, `AudioNonSilent`, `PerFileNonSilent`) when their spec
    /// declares `source = "spawn_only_files"` — see issue octos #1034.
    ///
    /// Absent (defaulted to an empty `Vec`) for non-spawn contexts and for
    /// spawn_only tools that emit no files. Validators that consult this
    /// list and find it empty surface a `Fail` outcome so misconfigured
    /// policies (e.g. opting into `spawn_only_files` from a turn-end
    /// validator that has no plugin output) are caught early.
    pub spawn_only_files: Vec<PathBuf>,
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
            tool_output: None,
            spawn_only_files: Vec::new(),
        }
    }

    /// Attach spawn task input args for `${args.<key>}` template
    /// interpolation by domain validators.
    pub fn with_input_args(mut self, args: serde_json::Value) -> Self {
        self.input_args = Some(args);
        self
    }

    /// Attach spawn task tool output (the `named_outputs` map) for
    /// `${output.<key>}` template interpolation by domain validators.
    pub fn with_tool_output(mut self, output: serde_json::Value) -> Self {
        self.tool_output = Some(output);
        self
    }

    /// Attach the plugin-reported `files_to_send` list so file-list-driven
    /// validators with `source = "spawn_only_files"` (issue octos #1034)
    /// can consume the authoritative path set the spawn_only tool emitted.
    pub fn with_spawn_only_files(mut self, files: Vec<PathBuf>) -> Self {
        self.spawn_only_files = files;
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
    /// True iff a non-`Pass` outcome from this validator demotes the spawn
    /// task — i.e. the originating [`Required::Hard`] tier. Soft/None map to
    /// `false` so legacy replay readers see them as warnings, matching the
    /// pre-Wave-3a `required: false` semantics.
    pub required: bool,
    /// Explicit gate-strength tier. Defaults to `"hard"` on replay of records
    /// emitted before Wave-3a so legacy ledgers de-serialize cleanly.
    #[serde(default = "default_required_tier")]
    pub required_tier: String,
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

    /// True iff this outcome's gate strength is the soft tier (added in
    /// Wave-3a so partial-artifact contracts can warn-and-continue without
    /// demoting the spawn task).
    pub fn is_soft_warning(&self) -> bool {
        self.required_tier == "soft" && !matches!(self.status, ValidatorStatus::Pass)
    }

    /// Backfill `required_tier` on records emitted before Wave-3a.
    ///
    /// Pre-Wave-3a ledger rows have no `required_tier` field, so
    /// `serde(default)` initializes it to the empty-string sentinel.
    /// Normalize the sentinel back to a tier derived from the legacy
    /// `required` field — `required: true` → `"hard"`, `required: false` →
    /// `"none"`. Idempotent: a Wave-3a-emitted record that already carries
    /// `"hard"`/`"soft"`/`"none"` is left untouched.
    fn normalize_legacy_tier(&mut self) {
        if self.required_tier.is_empty() {
            self.required_tier = if self.required { "hard" } else { "none" }.to_string();
        }
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
            let mut outcome: ValidatorOutcome = serde_json::from_str(&line)
                .wrap_err_with(|| format!("parse ledger line failed: {line}"))?;
            outcome.normalize_legacy_tier();
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
            // #1607 (P2): only snapshot tools the active provider policy would
            // let the model call. `MapToolDispatcher::dispatch` invokes
            // `Tool::execute` directly, bypassing `ToolRegistry::execute`'s
            // provider-policy gate — so without this filter a nested-project
            // `ToolCall` validator could invoke a tool the provider policy
            // denies. A denied tool is simply not inserted, so `dispatch`
            // returns "tool not registered for validator dispatch". With no
            // provider policy set (the default) every tool passes through.
            if !registry.provider_policy_permits(&name) {
                continue;
            }
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
    /// Optional sandbox for **command** validators (which shell out). When set,
    /// each command validator is wrapped via [`Sandbox::wrap_command`] so an
    /// untrusted workspace `.octos-workspace.toml` can't run unsandboxed host
    /// commands post-turn. `None` = run directly (current default; trusted
    /// callers). Opted into on the `mcp-serve` path.
    sandbox: Option<Arc<dyn Sandbox>>,
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
            sandbox: None,
        }
    }

    /// Override the directory where evidence files are written.
    pub fn with_evidence_root(mut self, path: impl Into<PathBuf>) -> Self {
        self.evidence_root = path.into();
        self
    }

    /// Run **command** validators under `sandbox` (wraps each via
    /// [`Sandbox::wrap_command`]). Used by callers that must confine
    /// workspace-declared command validators (e.g. `mcp-serve`).
    pub fn with_sandbox(mut self, sandbox: Arc<dyn Sandbox>) -> Self {
        self.sandbox = Some(sandbox);
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
                ValidatorSpec::AudioNonSilent {
                    glob,
                    min_ratio,
                    source,
                    extension,
                } => self.run_audio_non_silent(
                    invocation,
                    validator,
                    glob,
                    *min_ratio,
                    *source,
                    extension.as_deref(),
                    started_at,
                    started,
                ),
                ValidatorSpec::PerFileNonSilent {
                    glob,
                    min_ratio,
                    require_at_least,
                    source,
                    extension,
                } => self.run_per_file_non_silent(
                    invocation,
                    validator,
                    glob,
                    *min_ratio,
                    *require_at_least,
                    *source,
                    extension.as_deref(),
                    started_at,
                    started,
                ),
                ValidatorSpec::MagicBytes {
                    glob,
                    format,
                    source,
                    extension,
                } => self.run_magic_bytes(
                    invocation,
                    validator,
                    glob,
                    *format,
                    *source,
                    extension.as_deref(),
                    started_at,
                    started,
                ),
                ValidatorSpec::HttpProbeUntil {
                    url_template,
                    expected_status,
                    expected_contains,
                    poll_interval_ms,
                    deadline_ms,
                } => {
                    self.run_http_probe_until(
                        invocation,
                        validator,
                        url_template,
                        *expected_status,
                        expected_contains.as_deref(),
                        *poll_interval_ms,
                        *deadline_ms,
                        started_at,
                        started,
                    )
                    .await
                }
                ValidatorSpec::Sha256Match { glob, sha256 } => {
                    self.run_sha256_match(invocation, validator, glob, sha256, started_at, started)
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

        // Interpolate `${args.X}` and `${output.X}` references in both the
        // executable path and each argv element. argv elements are passed
        // as separate slots (no shell concatenation), so substitution is
        // safe — and necessary if a policy wants to reference a tool-
        // emitted path (e.g. `${output.patch_path}` for a `git apply`
        // check). A missing key surfaces as an Error outcome.
        let resolved_cmd = match interpolate_template(
            cmd,
            invocation.input_args.as_ref(),
            invocation.tool_output.as_ref(),
        ) {
            Ok(value) => value,
            Err(reason) => {
                return error_outcome(invocation, validator, started_at, started, reason);
            }
        };
        let mut resolved_args = Vec::with_capacity(args.len());
        for arg in args {
            match interpolate_template(
                arg,
                invocation.input_args.as_ref(),
                invocation.tool_output.as_ref(),
            ) {
                Ok(value) => resolved_args.push(value),
                Err(reason) => {
                    return error_outcome(invocation, validator, started_at, started, reason);
                }
            }
        }

        // Shell-safety layer: SafePolicy denies the known-dangerous patterns.
        let command_string = build_command_string(&resolved_cmd, &resolved_args);
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

        // A configured, real (non-no-op) sandbox means the validator MUST run
        // inside it. On POSIX the backend runs via `sh -c`, so we shell-quote the
        // argv with `shlex`. On Windows the backends shell out via `cmd /C`, for
        // which there is no safe POSIX argv wrapping here — and a workspace policy
        // can name an arbitrary executable, not merely inject an argument — so
        // running the argv directly would *bypass* the sandbox. Fail closed there
        // rather than escape it. A no-op sandbox (NoSandbox, or a backend whose
        // helper is unavailable) and the no-sandbox case have nothing to escape,
        // so they run the argv directly, where `Command` passes each element as a
        // distinct token (injection-safe).
        //
        // #1607 (codex-review follow-up): two real backends cannot safely run
        // the validator argv here, so a real one of either must FAIL CLOSED
        // rather than mis-quote or escape to the host:
        //  - Windows (`cmd /C`) ignores the POSIX single quotes `shlex` emits,
        //    so metacharacters in an argument would stay active.
        //  - Docker bind-mounts the workspace at a fixed in-container path
        //    (`/workspace`), but `Command` validators interpolate absolute
        //    *host* paths (e.g. `${output.patch_path}`) that don't resolve
        //    inside the container. Running them on the host instead (the prior
        //    behaviour) would bypass Docker's mount/write/network confinement —
        //    the very escape #1607 closes. Full in-container path translation
        //    is a documented follow-up (see `Sandbox::is_docker`).
        // A no-op / absent sandbox has nothing to escape and runs the argv
        // directly (injection-safe). ToolCall / file-check validators never
        // shell out, so they are unaffected either way.
        let real_sandbox = self.sandbox.as_ref().filter(|s| !s.is_noop());
        let is_windows = cfg!(windows);
        let cannot_wrap = real_sandbox.is_some_and(|s| is_windows || s.is_docker());
        if cannot_wrap {
            return error_outcome(
                invocation,
                validator,
                started_at,
                started,
                format!(
                    "command validator not supported under the active sandbox \
                     backend (Windows/Docker): {command_string}"
                ),
            );
        }
        let mut command = match real_sandbox {
            Some(sandbox) => {
                // wrap_command runs the string via `sh -c`, so shell-quote each
                // argv element (build_command_string above is unquoted — it's
                // only for the SafePolicy display/check). Otherwise `sh -c "sh -c
                // exit 1"`-style validators would word-split and mis-report.
                let quoted = match shlex::try_join(
                    std::iter::once(resolved_cmd.as_str())
                        .chain(resolved_args.iter().map(String::as_str)),
                ) {
                    Ok(q) => q,
                    Err(_) => {
                        return error_outcome(
                            invocation,
                            validator,
                            started_at,
                            started,
                            format!("command validator contains a NUL byte: {command_string}"),
                        );
                    }
                };
                sandbox.wrap_command(&quoted, &invocation.workspace_root)
            }
            None => {
                let mut c = Command::new(&resolved_cmd);
                c.args(&resolved_args)
                    .current_dir(&invocation.workspace_root);
                c
            }
        };
        command
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
                    required: validator.tier().is_hard(),
                    required_tier: validator.tier().as_str().to_string(),
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
                    required: validator.tier().is_hard(),
                    required_tier: validator.tier().as_str().to_string(),
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
                    required: validator.tier().is_hard(),
                    required_tier: validator.tier().as_str().to_string(),
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
                    required: validator.tier().is_hard(),
                    required_tier: validator.tier().as_str().to_string(),
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
        // Mirror the `HttpProbe` template story so policies can declare e.g.
        // `voice_profiles/${args.name}.wav` for `fm_voice_save` and have the
        // path resolved against the spawn task's input args. A missing key is
        // a hard error so the validator surfaces an `Error` outcome rather
        // than silently checking a literal `${args.name}` path. Note the
        // returned string is percent-encoded path-segment-safe — fine for the
        // single-filename segment use case the contract specifies.
        //
        // `${output.X}` resolves against the spawn task's `named_outputs` map
        // (verbatim, not percent-encoded) so a tool that emits a structured
        // path can drive the FileExists check directly.
        let resolved_path = match interpolate_template(
            path,
            invocation.input_args.as_ref(),
            invocation.tool_output.as_ref(),
        ) {
            Ok(resolved) => resolved,
            Err(reason) => {
                return error_outcome(invocation, validator, started_at, started, reason);
            }
        };
        let target = if Path::new(&resolved_path).is_absolute() {
            PathBuf::from(&resolved_path)
        } else {
            invocation.workspace_root.join(&resolved_path)
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
            required: validator.tier().is_hard(),
            required_tier: validator.tier().as_str().to_string(),
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
    /// Interpolates `${args.<key>}` and `${output.<key>}` against the spawn
    /// task's input args and `named_outputs` respectively, then performs a
    /// GET against the resulting URL and asserts the status code (and
    /// optionally a substring of the body, which is itself interpolated)
    /// matches the spec.
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
        let url = match interpolate_template(
            url_template,
            invocation.input_args.as_ref(),
            invocation.tool_output.as_ref(),
        ) {
            Ok(url) => url,
            Err(reason) => {
                return error_outcome(invocation, validator, started_at, started, reason);
            }
        };

        // Interpolate the body-substring assertion too so policies can
        // reference tool-emitted values (e.g. expected_contains carrying
        // `${output.repo}` to assert the deployment HTML mentions the
        // emitted repo slug).
        let resolved_contains = match expected_contains {
            Some(raw) => match interpolate_template(
                raw,
                invocation.input_args.as_ref(),
                invocation.tool_output.as_ref(),
            ) {
                Ok(value) => Some(value),
                Err(reason) => {
                    return error_outcome(invocation, validator, started_at, started, reason);
                }
            },
            None => None,
        };
        let expected_contains_ref = resolved_contains.as_deref();

        match probe_http(&url, timeout_ms, expected_status, expected_contains_ref).await {
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
    #[allow(clippy::too_many_arguments)]
    fn run_audio_non_silent(
        &self,
        invocation: &ValidatorInvocation,
        validator: &Validator,
        pattern: &str,
        min_ratio: f32,
        source: ValidatorFileSource,
        extension: Option<&str>,
        started_at: DateTime<Utc>,
        started: Instant,
    ) -> ValidatorOutcome {
        // Resolve the candidate file list. For `source = "glob"` (legacy
        // default) the validator interpolates the glob and matches it
        // against the workspace root. For `source = "spawn_only_files"`
        // (issue octos #1034) the validator consumes the plugin-reported
        // `files_to_send` list verbatim, optionally narrowed by extension —
        // see [`resolve_validator_files`] for the shared resolution rules.
        let (matches, pattern_for_diagnostics) = match resolve_validator_files(
            invocation,
            pattern,
            source,
            extension,
            FileResolutionMode::TemplateBoth,
            "audio_non_silent",
        ) {
            Ok(resolution) => resolution,
            Err(FileResolutionError { status, reason }) => {
                return self
                    .make_outcome(invocation, validator, status, reason, started_at, started);
            }
        };
        if matches.is_empty() {
            return self.make_outcome(
                invocation,
                validator,
                ValidatorStatus::Fail,
                format!("audio_non_silent: no files matched '{pattern_for_diagnostics}'"),
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

    /// Run a [`ValidatorSpec::PerFileNonSilent`] validator: every matched
    /// file must independently pass the non-silent ratio threshold, and the
    /// match count must meet `require_at_least`.
    ///
    /// Complements [`Self::run_audio_non_silent`], which only requires a
    /// single match. Reuses the WAV/MP3 decoder via
    /// [`decode_non_silent_ratio`]. Failure messages include the file's
    /// basename (NOT the full path) so an LLM logger can reason about which
    /// segment failed without leaking workspace layout.
    #[allow(clippy::too_many_arguments)]
    fn run_per_file_non_silent(
        &self,
        invocation: &ValidatorInvocation,
        validator: &Validator,
        pattern: &str,
        min_ratio: f32,
        require_at_least: usize,
        source: ValidatorFileSource,
        extension: Option<&str>,
        started_at: DateTime<Utc>,
        started: Instant,
    ) -> ValidatorOutcome {
        // Resolve the candidate file list. Glob mode interpolates `${args.X}`
        // ONLY (rejects path-traversal segments and absolute-path arg
        // values). `${output.X}` is intentionally not supported in glob mode
        // here — callers wanting tool-output-driven globs should use the
        // whole-file `AudioNonSilent` variant. `spawn_only_files` mode
        // (octos #1034) bypasses interpolation entirely and consumes the
        // plugin-reported file list verbatim.
        let (matches, _resolved_pattern) = match resolve_validator_files(
            invocation,
            pattern,
            source,
            extension,
            FileResolutionMode::TemplateArgsOnly,
            "per_file_non_silent",
        ) {
            Ok(resolution) => resolution,
            Err(FileResolutionError { status, reason }) => {
                return self
                    .make_outcome(invocation, validator, status, reason, started_at, started);
            }
        };

        // Enforce `require_at_least` as a hard floor on matched count
        // FIRST — that lets us distinguish "tool emitted zero artifacts"
        // (a true contract failure when the operator declared a minimum)
        // from "tool emitted artifacts but one is silent" (the per-file
        // gate below). The two failure modes warrant different remediation
        // hints in the ledger.
        if matches.len() < require_at_least {
            return self.make_outcome(
                invocation,
                validator,
                ValidatorStatus::Fail,
                format!(
                    "per_file_non_silent: expected >={require_at_least} audio files, found {}",
                    matches.len()
                ),
                started_at,
                started,
            );
        }

        let mut failures = Vec::new();
        for path in &matches {
            // Per-file basename for diagnostics. We deliberately avoid
            // emitting the full absolute path so the failure message stays
            // stable across hosts and doesn't surface a workspace temp
            // directory (which leaks across CI runs).
            let label = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("<unnamed>")
                .to_string();
            match decode_non_silent_ratio(path) {
                Ok(ratio) if ratio >= min_ratio => {}
                Ok(ratio) => failures.push(format!(
                    "{label}: non_silent_ratio={ratio:.3} < min_ratio={min_ratio:.3}"
                )),
                Err(reason) => failures.push(format!("{label}: {reason}")),
            }
        }

        if failures.is_empty() {
            self.make_outcome(
                invocation,
                validator,
                ValidatorStatus::Pass,
                format!(
                    "per_file_non_silent: all {} match(es) met min_ratio={min_ratio:.3}",
                    matches.len()
                ),
                started_at,
                started,
            )
        } else {
            self.make_outcome(
                invocation,
                validator,
                ValidatorStatus::Fail,
                format!("per_file_non_silent failed: {}", failures.join("; ")),
                started_at,
                started,
            )
        }
    }

    /// Run a MagicBytes validator.
    #[allow(clippy::too_many_arguments)]
    fn run_magic_bytes(
        &self,
        invocation: &ValidatorInvocation,
        validator: &Validator,
        pattern: &str,
        kind: MagicByteKind,
        source: ValidatorFileSource,
        extension: Option<&str>,
        started_at: DateTime<Utc>,
        started: Instant,
    ) -> ValidatorOutcome {
        // Resolve the candidate file list. Glob mode interpolates
        // `${args.X}` / `${output.X}` so policies can pin the glob to a
        // tool-emitted output path. `spawn_only_files` mode (octos #1034)
        // bypasses interpolation and consumes the plugin-reported file list
        // verbatim.
        let (matches, pattern_for_diagnostics) = match resolve_validator_files(
            invocation,
            pattern,
            source,
            extension,
            FileResolutionMode::TemplateBoth,
            "magic_bytes",
        ) {
            Ok(resolution) => resolution,
            Err(FileResolutionError { status, reason }) => {
                return self
                    .make_outcome(invocation, validator, status, reason, started_at, started);
            }
        };
        if matches.is_empty() {
            return self.make_outcome(
                invocation,
                validator,
                ValidatorStatus::Fail,
                format!("magic_bytes: no files matched '{pattern_for_diagnostics}'"),
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

    /// Run a polling [`HttpProbeUntil`] validator.
    ///
    /// Repeatedly probes the interpolated URL on a fixed cadence until the
    /// expected status+substring contract holds, or the wall-clock deadline
    /// expires. Each probe re-uses the [`probe_http`] helper that
    /// [`HttpProbe`] uses, so SSRF posture and timeout semantics match the
    /// single-shot variant. Per-probe timeout defaults to the smaller of the
    /// poll interval and 5s so a stuck probe never starves the loop.
    #[allow(clippy::too_many_arguments)]
    async fn run_http_probe_until(
        &self,
        invocation: &ValidatorInvocation,
        validator: &Validator,
        url_template: &str,
        expected_status: u16,
        expected_contains: Option<&str>,
        poll_interval_ms: u64,
        deadline_ms: u64,
        started_at: DateTime<Utc>,
        started: Instant,
    ) -> ValidatorOutcome {
        let url = match interpolate_template(
            url_template,
            invocation.input_args.as_ref(),
            invocation.tool_output.as_ref(),
        ) {
            Ok(url) => url,
            Err(reason) => {
                return error_outcome(invocation, validator, started_at, started, reason);
            }
        };

        // Per-probe timeout caps how long a single HTTP attempt can stall
        // before the polling loop reclaims control. Clamp to [1s, 5s] so
        // (a) the probe always has enough budget to complete the TCP
        // handshake even when `poll_interval_ms` is sub-second, and (b) a
        // hung probe never overshoots the wall-clock deadline by more than
        // 5s. We additionally cap each probe by the *remaining* deadline
        // inside the loop so the validator never runs past `deadline_ms`.
        let per_probe_timeout_floor_ms = poll_interval_ms.clamp(1_000, 5_000);
        let deadline = std::time::Instant::now() + Duration::from_millis(deadline_ms);
        let interval = Duration::from_millis(poll_interval_ms);

        let mut attempt: u32 = 0;
        let mut last_summary = "no response yet".to_string();
        loop {
            // Top-of-loop deadline guard: surface a Fail with the last
            // response summary as soon as the wall-clock deadline is hit,
            // even if `probe_http` would otherwise consume another timeout
            // budget. Critical for short deadlines (< per-probe floor).
            let now = std::time::Instant::now();
            let remaining_ms = deadline.saturating_duration_since(now).as_millis() as u64;
            if remaining_ms == 0 {
                return self.make_outcome(
                    invocation,
                    validator,
                    ValidatorStatus::Fail,
                    format!(
                        "http_probe_until {url} did not match in {deadline_ms}ms; last response: {last_summary}"
                    ),
                    started_at,
                    started,
                );
            }
            // Bound the per-probe timeout by remaining deadline so a hung
            // final probe cannot push the validator past `deadline_ms`.
            let per_probe_timeout_ms = per_probe_timeout_floor_ms.min(remaining_ms.max(1));

            attempt += 1;
            match probe_http(
                &url,
                per_probe_timeout_ms,
                expected_status,
                expected_contains,
            )
            .await
            {
                Ok(reason) => {
                    return self.make_outcome(
                        invocation,
                        validator,
                        ValidatorStatus::Pass,
                        format!("http_probe_until matched on attempt {attempt}: {reason}"),
                        started_at,
                        started,
                    );
                }
                Err(HttpProbeFailure::Timeout) => {
                    last_summary = format!("attempt {attempt}: per-probe timeout");
                }
                Err(HttpProbeFailure::Fail(reason)) => {
                    last_summary = format!("attempt {attempt}: {reason}");
                }
                Err(HttpProbeFailure::Error(reason)) => {
                    last_summary = format!("attempt {attempt}: transport error: {reason}");
                }
            }

            let now = std::time::Instant::now();
            if now >= deadline {
                return self.make_outcome(
                    invocation,
                    validator,
                    ValidatorStatus::Fail,
                    format!(
                        "http_probe_until {url} did not match in {deadline_ms}ms; last response: {last_summary}"
                    ),
                    started_at,
                    started,
                );
            }
            let remaining = deadline.saturating_duration_since(now);
            tokio::time::sleep(interval.min(remaining)).await;
        }
    }

    /// Run a [`Sha256Match`] validator.
    ///
    /// Resolves BOTH `glob` and `sha256` against `${args.<key>}` first so a
    /// tool that passes its expected hash through input args can scope the
    /// check to a per-invocation artifact path (e.g. the freshly-installed
    /// skill binary) and wire a manifest-derived checksum into the contract
    /// in one step. Each file matching the glob must have a digest equal to
    /// the (lowercased, hex) expected value; a single mismatch is a [`Fail`].
    /// Empty glob is a [`Fail`] so a contract that expects an artifact under
    /// a path never silently passes.
    fn run_sha256_match(
        &self,
        invocation: &ValidatorInvocation,
        validator: &Validator,
        glob_template: &str,
        sha256_template: &str,
        started_at: DateTime<Utc>,
        started: Instant,
    ) -> ValidatorOutcome {
        let expected_hex = match interpolate_template(
            sha256_template,
            invocation.input_args.as_ref(),
            invocation.tool_output.as_ref(),
        ) {
            Ok(value) => value.trim().to_ascii_lowercase(),
            Err(reason) => {
                return error_outcome(invocation, validator, started_at, started, reason);
            }
        };
        if expected_hex.len() != 64 || !expected_hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return error_outcome(
                invocation,
                validator,
                started_at,
                started,
                format!(
                    "sha256_match: expected hex-encoded 64-char SHA-256 digest, got '{expected_hex}'"
                ),
            );
        }
        // Interpolate the glob so a per-invocation contract (e.g.
        // `${args.skill_dir}/main`) can scope the digest check to the
        // artifact this specific spawn task produced. Uses the path-safe
        // interpolator so embedded `/` separators survive — operators
        // pinning a literal glob like `skills/*/main` are unaffected.
        let pattern = match interpolate_args_path(glob_template, invocation.input_args.as_ref()) {
            Ok(pattern) => pattern,
            Err(reason) => {
                return error_outcome(invocation, validator, started_at, started, reason);
            }
        };
        let matches = match glob_files(&invocation.workspace_root, &pattern) {
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
                format!("sha256_match: no files matched '{pattern}'"),
                started_at,
                started,
            );
        }
        let mut failures = Vec::new();
        for path in &matches {
            match compute_sha256_hex(path) {
                Ok(actual) => {
                    if actual != expected_hex {
                        failures.push(format!(
                            "{}: actual={actual} != expected={expected_hex}",
                            path.display()
                        ));
                    }
                }
                Err(reason) => failures.push(format!("{}: {reason}", path.display())),
            }
        }
        if failures.is_empty() {
            self.make_outcome(
                invocation,
                validator,
                ValidatorStatus::Pass,
                format!(
                    "sha256_match: all {} match(es) carry expected digest {expected_hex}",
                    matches.len()
                ),
                started_at,
                started,
            )
        } else {
            self.make_outcome(
                invocation,
                validator,
                ValidatorStatus::Fail,
                format!("sha256_match failed: {}", failures.join("; ")),
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
            required: validator.tier().is_hard(),
            required_tier: validator.tier().as_str().to_string(),
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
/// This thin wrapper preserves the legacy single-source signature for the
/// inline test suite; production callers use [`interpolate_template`] which
/// also resolves `${output.<key>}` against the spawn task's `named_outputs`.
/// See [`interpolate_template`] for the canonical doc-comment on
/// percent-encoding semantics and missing-key error policy.
///
/// Use [`interpolate_args_path`] for glob/path templates where
/// percent-encoding would break the segment separator.
#[cfg(test)]
fn interpolate_args(
    template: &str,
    input_args: Option<&serde_json::Value>,
) -> Result<String, String> {
    interpolate_template(template, input_args, None)
}

/// Substitute `${args.<key>}` and `${output.<key>}` references in `template`.
///
/// Two interpolation sources, two trust levels:
///
/// - `${args.<key>}` resolves against the originating spawn task's input
///   args (LLM-controlled). Values are percent-encoded into URL-segment-safe
///   form so an LLM-supplied value cannot break out of the path/query slot
///   it lands in.
/// - `${output.<key>}` resolves against the spawn task tool's `named_outputs`
///   map (tool-controlled, trust boundary equal to the tool itself). Values
///   are spliced in verbatim because the canonical use case is a tool that
///   emits a full URL (e.g. `mofa_publish` emitting `deploy_url`) that the
///   downstream HTTP probe needs to call exactly as-is. Percent-encoding
///   would corrupt the URL.
///
/// A missing key in either source surfaces as `Error` outcome (matches the
/// `${args.X}` semantics shipped in #935): the validator runner translates
/// this `Err` into a typed Error result rather than silently substituting
/// the empty string.
///
/// Mixed templates resolve both sources in a single pass, e.g.
/// `https://${output.host}/voices/${args.name}` works in one call. Order
/// inside the template is preserved.
fn interpolate_template(
    template: &str,
    input_args: Option<&serde_json::Value>,
    tool_output: Option<&serde_json::Value>,
) -> Result<String, String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    loop {
        let next_args = rest.find("${args.");
        let next_output = rest.find("${output.");
        let (start, prefix_len, source, source_label, encode) = match (next_args, next_output) {
            (None, None) => break,
            (Some(a), None) => (a, "${args.".len(), input_args, "input arg", true),
            (None, Some(o)) => (o, "${output.".len(), tool_output, "output", false),
            (Some(a), Some(o)) => {
                if a <= o {
                    (a, "${args.".len(), input_args, "input arg", true)
                } else {
                    (o, "${output.".len(), tool_output, "output", false)
                }
            }
        };
        out.push_str(&rest[..start]);
        let after = &rest[start + prefix_len..];
        let end = after
            .find('}')
            .ok_or_else(|| format!("unterminated reference in template: {template}"))?;
        let key = &after[..end];
        let value = input_arg(source, key).ok_or_else(|| {
            format!("{source_label} '{key}' not found while interpolating template: {template}")
        })?;
        if encode {
            out.push_str(&percent_encode_url_segment(&value));
        } else {
            out.push_str(&value);
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Path-safe counterpart to [`interpolate_args`] for glob templates.
///
/// Same key lookup semantics, but values are inserted verbatim instead of
/// percent-encoded so embedded `/` separators (e.g. `${args.skill_dir}` =
/// `"skills/example"`) survive into the resulting glob. Path traversal
/// attempts that try to escape the workspace root are rejected explicitly so
/// an LLM-controlled arg value cannot wire the validator at, say, a
/// `${args.skill_dir}/main` template that resolves to `/etc/passwd`. The
/// caller is expected to thread the resulting glob through `glob_files`
/// (which resolves relative paths against the workspace root, blunting
/// further traversal).
fn interpolate_args_path(
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
        // Reject path-traversal segments so an LLM-controlled arg value can
        // never escape the workspace root via `${args.X}/...`. We
        // intentionally accept `/` as a segment separator (otherwise common
        // contracts like `${args.skill_dir}/main` can't work) but block
        // `..` segments and absolute-path leakage.
        for segment in value.split('/') {
            if segment == ".." {
                return Err(format!(
                    "input arg '{key}' contains '..' segment which is rejected for path templates: {value}"
                ));
            }
        }
        if value.starts_with('/') {
            return Err(format!(
                "input arg '{key}' must be a relative path, got absolute: {value}"
            ));
        }
        out.push_str(&value);
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

/// Template-interpolation flavour used by [`resolve_validator_files`].
///
/// Mirrors the per-variant interpolation rules already in place before octos
/// #1034: `MagicBytes` / `AudioNonSilent` substitute both `${args.X}` and
/// `${output.X}`, while `PerFileNonSilent` substitutes `${args.X}` only via
/// the path-traversal-safe [`interpolate_args_path`] helper.
#[derive(Clone, Copy, Debug)]
enum FileResolutionMode {
    /// Interpolate both `${args.X}` and `${output.X}`. Used by `MagicBytes`
    /// and `AudioNonSilent`.
    TemplateBoth,
    /// Interpolate `${args.X}` only via [`interpolate_args_path`]. Used by
    /// `PerFileNonSilent`.
    TemplateArgsOnly,
}

/// Error returned by [`resolve_validator_files`] when the file list cannot be
/// produced (interpolation failure, invalid glob, etc.). The caller surfaces
/// `status` + `reason` directly through `make_outcome`.
#[derive(Clone, Debug)]
struct FileResolutionError {
    status: ValidatorStatus,
    reason: String,
}

/// Resolve the candidate file list for a file-list-driven validator
/// (`MagicBytes`, `AudioNonSilent`, `PerFileNonSilent`).
///
/// * `ValidatorFileSource::Glob` (legacy default) — interpolates the
///   `pattern` per `mode` and matches the resolved glob against
///   `invocation.workspace_root` via [`glob_files`]. Returns the matched
///   path list and the resolved pattern (for diagnostics).
/// * `ValidatorFileSource::SpawnOnlyFiles` (octos #1034) — bypasses the glob
///   entirely and consumes `invocation.spawn_only_files` verbatim. If
///   `extension` is set, only files whose extension matches (case-
///   insensitive, no leading dot) are returned. The diagnostic pattern is
///   synthesized from the optional extension filter so failure messages
///   remain interpretable. Files that do NOT exist on disk are filtered out
///   so the contract treats a stale `files_to_send` entry the same way the
///   glob path treats a missing match.
fn resolve_validator_files(
    invocation: &ValidatorInvocation,
    pattern: &str,
    source: ValidatorFileSource,
    extension: Option<&str>,
    mode: FileResolutionMode,
    validator_kind: &str,
) -> Result<(Vec<PathBuf>, String), FileResolutionError> {
    match source {
        ValidatorFileSource::Glob => {
            let resolved_pattern = match mode {
                FileResolutionMode::TemplateBoth => interpolate_template(
                    pattern,
                    invocation.input_args.as_ref(),
                    invocation.tool_output.as_ref(),
                ),
                FileResolutionMode::TemplateArgsOnly => {
                    interpolate_args_path(pattern, invocation.input_args.as_ref())
                }
            }
            .map_err(|reason| FileResolutionError {
                status: ValidatorStatus::Error,
                reason,
            })?;
            let matches =
                glob_files(&invocation.workspace_root, &resolved_pattern).map_err(|reason| {
                    FileResolutionError {
                        status: ValidatorStatus::Error,
                        reason,
                    }
                })?;
            Ok((matches, resolved_pattern))
        }
        ValidatorFileSource::SpawnOnlyFiles => {
            let normalized_ext = extension.map(normalize_extension_suffix);
            let mut matches = Vec::new();
            for path in &invocation.spawn_only_files {
                if let Some(ref required) = normalized_ext {
                    let actual = path
                        .extension()
                        .and_then(|e| e.to_str())
                        .map(|s| s.to_ascii_lowercase())
                        .unwrap_or_default();
                    if &actual != required {
                        continue;
                    }
                }
                if path.is_file() {
                    matches.push(path.clone());
                }
            }
            let diagnostic_pattern = match normalized_ext.as_deref() {
                Some(ext) => format!("spawn_only_files (extension={ext})"),
                None => "spawn_only_files".to_string(),
            };
            // Surface a helpful Error outcome if a misconfigured policy
            // opts into `spawn_only_files` but the validator runs in a
            // context without any plugin-reported files (e.g. a turn-end
            // pass). The caller turns the empty list into a domain-specific
            // Fail message; this branch fires ONLY when the policy itself
            // is asking for a list the invocation cannot produce.
            if invocation.spawn_only_files.is_empty() {
                return Err(FileResolutionError {
                    status: ValidatorStatus::Fail,
                    reason: format!(
                        "{validator_kind}: source = \"spawn_only_files\" requested but the spawn_only \
                         tool emitted no files_to_send"
                    ),
                });
            }
            Ok((matches, diagnostic_pattern))
        }
    }
}

/// Normalize an extension filter (`"mp3"`, `".MP3"`, `"Mp3"`) to a lowercase
/// no-leading-dot form so [`resolve_validator_files`] can compare against
/// `PathBuf::extension()` output uniformly.
fn normalize_extension_suffix(ext: &str) -> String {
    ext.trim_start_matches('.').to_ascii_lowercase()
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

/// Compute the SHA-256 digest of a file, streaming chunked reads so large
/// artifacts don't blow the validator process's memory budget. Returns the
/// lowercase hex encoding so callers can compare against the
/// canonical-form manifest digest with `==`.
fn compute_sha256_hex(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(|err| format!("open failed: {err}"))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buf)
            .map_err(|err| format!("read failed: {err}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
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
        required: validator.tier().is_hard(),
        required_tier: validator.tier().as_str().to_string(),
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
        ValidatorSpec::PerFileNonSilent { .. } => "per_file_non_silent",
        ValidatorSpec::MagicBytes { .. } => "magic_bytes",
        ValidatorSpec::HttpProbeUntil { .. } => "http_probe_until",
        ValidatorSpec::Sha256Match { .. } => "sha256_match",
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
        // The Wave-3a explicit tier — "hard" / "soft" / "none". Lets
        // operators dashboard soft warnings separately from purely optional
        // ones, even though both share `required = false`.
        "tier" => outcome.required_tier.clone(),
    )
    .increment(1);

    if outcome.required && outcome.status != ValidatorStatus::Pass {
        counter!("octos_workspace_validator_required_failed_total").increment(1);
    } else if !outcome.required && outcome.status != ValidatorStatus::Pass {
        counter!("octos_workspace_validator_optional_warning_total").increment(1);
        if outcome.required_tier == "soft" {
            counter!("octos_workspace_validator_soft_warning_total").increment(1);
        }
    }
}

/// Kill a child process (and process group on Unix) cleanly. Used by the
/// command validator timeout handler.
async fn kill_child_process(pid: u32) {
    debug!(pid, "killing validator child on timeout");

    #[cfg(unix)]
    {
        let has_own_process_group = process_group_id(pid).is_some_and(|pgid| pgid == pid);
        let mut descendants = collect_descendant_pids(pid);

        if has_own_process_group {
            signal_process_group(pid, "-15");
        }
        signal_pids(&descendants, "-15");
        signal_process(pid, "-15");

        tokio::time::sleep(KILL_GRACE_PERIOD).await;

        descendants.extend(collect_descendant_pids(pid));
        descendants.sort_unstable();
        descendants.dedup();

        let still_alive = process_exists(pid)
            || descendants
                .iter()
                .any(|descendant_pid| process_exists(*descendant_pid))
            || (has_own_process_group && process_group_exists(pid));
        if still_alive {
            if has_own_process_group {
                signal_process_group(pid, "-9");
            }
            signal_pids(&descendants, "-9");
            signal_process(pid, "-9");
        }
    }

    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .status();
    }
}

#[cfg(unix)]
fn signal_process_group(pid: u32, signal: &str) -> bool {
    use std::process::Command as StdCommand;

    let group = format!("-{pid}");
    StdCommand::new("kill")
        .args([signal, "--", &group])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(unix)]
fn signal_process(pid: u32, signal: &str) -> bool {
    use std::process::Command as StdCommand;

    StdCommand::new("kill")
        .args([signal, &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(unix)]
fn signal_pids(pids: &[u32], signal: &str) {
    for pid in pids {
        signal_process(*pid, signal);
    }
}

#[cfg(unix)]
fn process_group_exists(pid: u32) -> bool {
    signal_process_group(pid, "-0")
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    signal_process(pid, "-0")
}

#[cfg(unix)]
fn process_group_id(pid: u32) -> Option<u32> {
    use std::process::Command as StdCommand;

    let output = StdCommand::new("ps")
        .args(["-o", "pgid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .and_then(|pgid| pgid.parse().ok())
}

#[cfg(unix)]
fn collect_descendant_pids(root_pid: u32) -> Vec<u32> {
    use std::process::Command as StdCommand;

    let output = match StdCommand::new("ps").args(["-eo", "pid=,ppid="]).output() {
        Ok(output) if output.status.success() => output,
        _ => return Vec::new(),
    };

    let mut children_by_parent: std::collections::HashMap<u32, Vec<u32>> =
        std::collections::HashMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(ppid)) = (fields.next(), fields.next()) else {
            continue;
        };
        let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) else {
            continue;
        };
        children_by_parent.entry(ppid).or_default().push(pid);
    }

    let mut descendants = Vec::new();
    let mut stack = children_by_parent
        .get(&root_pid)
        .cloned()
        .unwrap_or_default();
    while let Some(pid) = stack.pop() {
        descendants.push(pid);
        if let Some(children) = children_by_parent.get(&pid) {
            stack.extend(children);
        }
    }
    descendants
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
#[path = "validators_tests.rs"]
mod tests;
