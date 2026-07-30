//! Module 2 — `run_attempt`: the per-attempt executor.
//!
//! It receives an already-launched `attempt_id` (the pool launches; this
//! runs it to a recorded terminal state) and:
//!
//! 1. CASes the attempt `Leased/Launching → Running` — a `Superseded` outcome
//!    is a lost race (`Aborted`), a store `Err` is infra (`RecordError`);
//! 2. builds a [`Task`] from the task's brief + acceptance criteria;
//! 3. mints a fresh closed-registry [`octos_agent::Agent`];
//! 4. runs it under a HARD [`tokio::time::timeout`] (the agent loop has no
//!    internal wall-clock deadline);
//! 5. maps the result to an [`AcceptanceVerdict`] — a timeout/infra-error is
//!    `Terminated`, a `success:false` stop is `Rejected`, a `success:true`
//!    run is gated on acceptance (`Accepted`/`Rejected`);
//! 6. records the real outcome + snapshot to the store, then unblocks
//!    dependents.
//!
//! # Known v1 limitations (documented, not yet fixed)
//!
//! - **Token under-commit on non-success paths (P2-2).** A timeout or infra
//!   error drops the [`octos_core::TaskResult`], so its [`TokenUsage`] is lost
//!   and the attempt commits `0` tokens against the fleet budget (see
//!   [`Computed::terminated`]). Upstream `turn_state` also does not fold
//!   `reasoning_tokens`, so even a committed count can under-report. This is a
//!   soft-budget v1 limitation (kernel spec §6): the budget is advisory, so an
//!   under-commit only weakens admission pressure, never correctness.
//!   Best-effort usage capture on the timeout path is a possible follow-up.
//! - **`split_whitespace` argv for `CommandExit` (P3).** A `CommandExit`
//!   verifier's command is split on whitespace into program + argv with NO
//!   shell, which is injection-resistant but quoting-naive: `test -f "out
//!   file.txt"` splits into four tokens and would falsely reject. Acceptance
//!   commands must use un-quoted, whitespace-free argv tokens for v1.
//! - **`FileExists` confinement is point-in-time (TOCTOU, P2).**
//!   [`check_file_exists_confined`] resolves + containment-checks the path
//!   (`canonicalize`) and then `metadata`s it as two separate syscalls; a
//!   concurrent writer could swap an in-workspace file for an external symlink
//!   between the two, and `metadata` would follow it. This is acceptable ONLY
//!   under v1's single-writer-per-task-workspace assumption: the confinement is
//!   a point-in-time check, not a race-proof absolute guarantee against a
//!   hostile concurrent mutator of the task's own workspace.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use octos_agent::sandbox::Sandbox;
use octos_agent::validators::{
    ValidatorInvocation, ValidatorOutcome, ValidatorPhase, ValidatorRunner, ValidatorStatus,
};
use octos_agent::workspace_policy::{Validator, ValidatorPhaseKind, ValidatorSpec};
use octos_core::{Message, Task, TaskContext, TaskKind, TaskResult, TokenUsage};
use octos_fleet::{
    AcceptanceVerdict, ChildResultSnapshot, CompleteOutcome, EvidenceRef, Fleet, FleetKernelStore,
    MarkRunningOutcome, TaskView, Verifier,
};

use crate::AgentFactory;

/// Minimum acceptance-phase budget (P1-5b floor). A run that finishes right at
/// the deadline still gets this small window to verify, rather than a 0ms
/// timeout. Real deadlines dwarf it, so it only binds at the deadline edge.
const ACCEPTANCE_MIN_BUDGET: Duration = Duration::from_secs(2);

/// Slack added on top of a command validator's own `timeout_ms` for the coarse
/// outer backstop (round-4 P1). The per-validator `timeout_ms` is what actually
/// kills the subprocess (its cancellation-safe internal `timeout` +
/// `kill_child_process`); the backstop only guards against a validator that
/// hangs OUTSIDE that awaited kill path, so it must be comfortably longer than
/// the validator's internal SIGTERM→SIGKILL grace or it would preempt the kill.
const ACCEPTANCE_BACKSTOP_GRACE: Duration = Duration::from_secs(2);

/// Terminal disposition of a single attempt run — returned for logging and
/// tests, never persisted (the durable record is written to the store).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptOutcome {
    /// The attempt completed and its verdict was recorded (child terminal).
    Completed { verdict: AcceptanceVerdict },
    /// The completion CAS found the attempt stale/superseded: its result was
    /// dropped and no state changed (deliberately not an error).
    Superseded,
    /// `mark_running` returned `Superseded` — the attempt was stale/superseded
    /// (a lost race) before the run started, so it is genuinely NOT ours: it
    /// was NOT completed and the pool disarms the guard. Recovery/relaunch
    /// reconciles it.
    Aborted { reason: String },
    /// A store CAS itself errored (`mark_running` or `complete_child` hit an
    /// I/O / parse / invariant break): the attempt may still be live and ours,
    /// so the pool KEEPS the guard armed (its `Drop` un-wedges the child) and
    /// the caller logs and lets recovery reconcile.
    RecordError { reason: String },
}

/// Run one launched attempt to a recorded terminal state. See the module
/// docs for the sequence. `actual_now_ms` is the wall clock threaded into
/// the store's `complete_child` (settlement timestamp) and the follow-on
/// readiness promotion.
#[allow(clippy::too_many_arguments)] // store + ids + view + factory + deadline + epoch + clock are irreducible
pub async fn run_attempt(
    store: Arc<FleetKernelStore>,
    fleet_id: &str,
    task_id: &str,
    attempt_id: &str,
    task_view: &TaskView,
    factory: &AgentFactory,
    working_dir: &Path,
    deadline: Duration,
    owner_epoch: u64,
    actual_now_ms: impl Fn() -> u64,
) -> AttemptOutcome {
    // 1. CAS Leased/Launching -> Running, distinguishing a genuine lost race
    //    from an infra error (round-4 P1):
    //    - `Superseded` — the attempt is stale/superseded or not the child's
    //      current one: genuinely NOT ours, so `Aborted` (the pool disarms the
    //      guard; relaunch/recovery owns the fallout).
    //    - `Err` — a real store/infra failure: the still-`Launching` attempt may
    //      still be ours, so `RecordError` (the pool KEEPS the guard armed so
    //      its `Drop` un-wedges the child). Never conflate the two.
    match store.mark_running(task_id, attempt_id).await {
        Ok(MarkRunningOutcome::Running) => {}
        Ok(MarkRunningOutcome::Superseded) => {
            tracing::warn!(
                %fleet_id, %task_id, %attempt_id,
                "fleet worker: mark_running superseded (attempt not ours); aborting without completing",
            );
            return AttemptOutcome::Aborted {
                reason: "mark_running superseded".to_string(),
            };
        }
        Err(err) => {
            tracing::error!(
                %fleet_id, %task_id, %attempt_id, error = %err,
                "fleet worker: mark_running infra error; keeping guard armed for drop-cleanup",
            );
            return AttemptOutcome::RecordError {
                reason: format!("mark_running errored: {err}"),
            };
        }
    }

    // 2. Build the Task from the rendered brief + acceptance criteria.
    let task = Task::new(
        TaskKind::Custom {
            name: "fleet_task".to_string(),
            params: serde_json::json!({
                "fleet_id": fleet_id,
                "task_id": task_id,
                "attempt_id": attempt_id,
            }),
        },
        TaskContext {
            working_dir: working_dir.to_path_buf(),
            working_memory: vec![Message::user(render_brief(task_view))],
            ..Default::default()
        },
    );

    // PR A: the sandbox's raw network egress comes from THIS task's grant, not
    // a hardcoded `false`. `None`/`Hosts` → no raw egress (the shell cannot
    // `curl`; `Hosts` is enforced by the granted web tools); `Full` → raw egress
    // (git/npm/etc.). The isolating backend is unchanged — only the network flag
    // is per-attempt.
    let allow_network = task_view.grant.network.allows_raw_egress();

    // P1-3-fix: ONE sandbox instance for the whole attempt, shared by the
    // agent's granted registry AND the acceptance validators — so a
    // non-idempotent factory can never sandbox the agent while handing the
    // validators a weaker (e.g. no-op) sandbox.
    let sandbox = factory.sandbox_for(working_dir, allow_network);

    // #1857 PR 5a fix (H1, codex round 2) — ATTEMPT-TIME fail-closed. The serve
    // boot probe checks ONE sandbox instance, but the factory reconstructs the
    // sandbox PER attempt and `SandboxMode::Auto` can fall through to `NoSandbox`
    // if the backend (e.g. bwrap) became unavailable AFTER boot. The closed tool
    // set is a denylist, not a boundary — the shell's network/host reach is
    // bounded ONLY by the sandbox — so a no-op sandbox here means running a fleet
    // worker unsandboxed. REFUSE: settle the attempt `Terminated` (via the normal
    // `complete_child` path below, so the child ends terminal, not silently
    // unsandboxed) WITHOUT ever building or running the agent.
    let computed = if sandbox.is_noop() {
        tracing::error!(
            %fleet_id, %task_id, %attempt_id,
            "fleet worker: no isolating sandbox available at attempt time; terminating the \
             attempt instead of running the agent unsandboxed",
        );
        Computed::terminated("no isolating sandbox available at attempt time".to_string())
    } else {
        // 3. Fresh, granted-registry agent (cannot park, cannot fan out; holds
        //    EXACTLY the operator-granted tools) under the shared sandbox. Its
        //    per-tool timeouts AND per-command shell ceiling are clamped to
        //    `deadline` (P1-2). An incoherent grant (rejected at parse) fails
        //    closed here too: terminate the attempt without running.
        match factory.build_agent(working_dir, sandbox.clone(), deadline, &task_view.grant) {
            Err(err) => {
                tracing::error!(
                    %fleet_id, %task_id, %attempt_id, error = %err,
                    "fleet worker: invalid worker grant; terminating the attempt",
                );
                Computed::terminated(format!("invalid worker grant: {err}"))
            }
            Ok(agent) => {
                // 4. Run under a HARD deadline — `run_task` is cancel-safe to
                //    drop and has NO internal wall-clock cap, so the timeout
                //    wrapper is mandatory.
                let start = Instant::now();
                let run = tokio::time::timeout(deadline, agent.run_task(&task)).await;
                let elapsed = start.elapsed();

                // 5-6. Map the result to a verdict + snapshot inputs.
                match run {
                    Err(_elapsed) => Computed::terminated(format!(
                        "deadline exceeded after {}s",
                        deadline.as_secs()
                    )),
                    Ok(Err(report)) => Computed::terminated(format!("run failed: {report}")),
                    Ok(Ok(result)) if !result.success => {
                        let reason = if result.output.trim().is_empty() {
                            "run did not succeed".to_string()
                        } else {
                            result.output.clone()
                        };
                        Computed::rejected(reason, &result)
                    }
                    Ok(Ok(result)) => {
                        // P1-5b: bound the acceptance phase by the REMAINING
                        // deadline so a slow `CommandExit` validator can't hold
                        // both permits past the fleet deadline. A small floor
                        // keeps a right-at-the-edge run from getting a 0ms
                        // acceptance budget.
                        let remaining = deadline
                            .checked_sub(elapsed)
                            .unwrap_or(Duration::ZERO)
                            .max(ACCEPTANCE_MIN_BUDGET);
                        let (verdict, error) = run_acceptance(
                            task_view,
                            working_dir,
                            factory,
                            sandbox,
                            deadline.as_secs().max(1),
                            remaining,
                        )
                        .await;
                        Computed::from_verdict(verdict, error, &result)
                    }
                }
            }
        }
    };

    // 7. Record the real outcome + snapshot to the store DIRECTLY (not
    //    `Fleet::record_outcome`, which cannot carry the snapshot).
    let now = actual_now_ms();
    let snapshot = ChildResultSnapshot {
        output: computed.output,
        success: matches!(computed.verdict, AcceptanceVerdict::Accepted { .. }),
        tokens_used: computed.actual_tokens,
        files: computed.files,
        error: computed.error,
    };
    match store
        .complete_child(
            fleet_id,
            task_id,
            attempt_id,
            computed.verdict.clone(),
            snapshot,
            computed.actual_tokens,
            owner_epoch,
            now,
        )
        .await
    {
        Ok(CompleteOutcome::Completed) => {
            // Unblock dependents. `ready_tasks` is self-healing + atomic, so a
            // failure here is advisory — recovery/the keeper re-derives it.
            if let Err(err) = Fleet::bind(store.clone(), fleet_id).ready_tasks(now).await {
                tracing::warn!(
                    %fleet_id, error = %err,
                    "fleet worker: post-completion ready_tasks failed (advisory)",
                );
            }
            AttemptOutcome::Completed {
                verdict: computed.verdict,
            }
        }
        Ok(CompleteOutcome::Superseded) => AttemptOutcome::Superseded,
        Err(err) => {
            tracing::error!(
                %fleet_id, %task_id, %attempt_id, error = %err,
                "fleet worker: complete_child errored",
            );
            AttemptOutcome::RecordError {
                reason: err.to_string(),
            }
        }
    }
}

/// The verdict plus the fields fed into the [`ChildResultSnapshot`].
struct Computed {
    verdict: AcceptanceVerdict,
    output: String,
    files: Vec<String>,
    error: Option<String>,
    actual_tokens: u64,
}

impl Computed {
    /// Timeout / infra error — no `TaskResult` to draw from, so `0` tokens are
    /// committed (P2-2 soft-budget v1 limitation; see the module docs).
    fn terminated(reason: String) -> Self {
        Self {
            verdict: AcceptanceVerdict::Terminated {
                reason: reason.clone(),
            },
            output: String::new(),
            files: Vec::new(),
            error: Some(reason),
            actual_tokens: 0,
        }
    }

    /// A `success:false` run stop (budget / max-tokens / content-filter).
    fn rejected(reason: String, result: &TaskResult) -> Self {
        Self {
            verdict: AcceptanceVerdict::Rejected {
                reason: reason.clone(),
            },
            output: result.output.clone(),
            files: files_to_strings(result),
            error: Some(reason),
            actual_tokens: total_tokens(&result.token_usage),
        }
    }

    /// A `success:true` run mapped through the acceptance gate.
    fn from_verdict(
        verdict: AcceptanceVerdict,
        error: Option<String>,
        result: &TaskResult,
    ) -> Self {
        Self {
            verdict,
            output: result.output.clone(),
            files: files_to_strings(result),
            error,
            actual_tokens: total_tokens(&result.token_usage),
        }
    }
}

fn files_to_strings(result: &TaskResult) -> Vec<String> {
    result
        .files_modified
        .iter()
        .map(|p| p.display().to_string())
        .collect()
}

/// The meaningful token fields, summed: input + output + reasoning. Cache
/// read/write tokens are excluded (they are not fresh work).
fn total_tokens(usage: &TokenUsage) -> u64 {
    u64::from(usage.input_tokens)
        + u64::from(usage.output_tokens)
        + u64::from(usage.reasoning_tokens)
}

/// Render the task brief the worker agent sees: title + detail + the
/// acceptance criteria as text (so the agent knows what "done" means).
fn render_brief(task_view: &TaskView) -> String {
    let mut brief = format!("# Task: {}\n", task_view.title);
    if !task_view.detail.trim().is_empty() {
        brief.push('\n');
        brief.push_str(task_view.detail.trim());
        brief.push('\n');
    }
    if !task_view.acceptance.is_empty() {
        brief.push_str("\n## Acceptance criteria — ALL must hold\n");
        for crit in &task_view.acceptance {
            let how = match &crit.verifier {
                Verifier::FileExists { path } => format!("file `{path}` must exist"),
                Verifier::CommandExit { cmd, code } => {
                    format!("`{cmd}` must exit with code {code}")
                }
                Verifier::ValidatorRef { id } => format!("validator `{id}` must pass"),
                Verifier::Manual => "manual verification".to_string(),
            };
            brief.push_str(&format!("- {} ({})\n", crit.description, how));
        }
    }
    brief
}

/// Run the mechanical acceptance gate over the task's criteria. Returns the
/// verdict and, on rejection, a human-readable reason.
///
/// - `FileExists{path}` is evaluated IN THE WORKER (P1-5a) via
///   [`check_file_exists_confined`]: lexically confined, then `canonicalize`d
///   and asserted to stay UNDER the canonical workspace root as a regular file
///   — so an in-workspace symlink (`out.txt -> /etc/passwd`) cannot escape the
///   way the symlink-following `ValidatorRunner` FileExists check would.
/// - `CommandExit{cmd, code:0}` → a required [`ValidatorSpec::Command`] run
///   through the [`ValidatorRunner`] (split into program + argv, NO shell;
///   passes iff exit `0`, so a non-zero expected `code` is unsupported).
///
/// **Fail-closed (P1-5a):** any criterion the executor cannot mechanically
/// verify — `Manual`, `ValidatorRef`, `CommandExit{code != 0}`, an empty
/// command, an absent file, or a workspace-escaping `FileExists` — REJECTS the
/// gate. Criteria carry no `required` flag, so all are treated as required; we
/// never drop-to-pass on the agent's own self-report. Only a task with NO
/// criteria at all falls back to the run's own `success` as the gate.
///
/// Command validators run under the SHARED `sandbox` instance the agent ran
/// under (P1-3-fix / P1-5c), and the whole `CommandExit` phase is bounded by
/// `remaining` (P1-5b) so it can't hold permits past the fleet deadline.
async fn run_acceptance(
    task_view: &TaskView,
    workspace_root: &Path,
    factory: &AgentFactory,
    sandbox: Arc<dyn Sandbox>,
    max_shell_timeout_secs: u64,
    mut remaining: Duration,
) -> (AcceptanceVerdict, Option<String>) {
    let mut command_validators: Vec<Validator> = Vec::new();
    let mut evidence: Vec<EvidenceRef> = Vec::new();
    // Fail-closed: criteria we cannot mechanically verify (or that fail /
    // escape the workspace) reject the gate — we never drop-to-pass.
    let mut unverifiable: Vec<String> = Vec::new();

    for crit in &task_view.acceptance {
        match &crit.verifier {
            Verifier::FileExists { path } => {
                match check_file_exists_confined(workspace_root, path) {
                    Ok(Some(canonical)) => evidence.push(EvidenceRef {
                        kind: "file_exists".to_string(),
                        locator: canonical.display().to_string(),
                        sha256: String::new(),
                        captured_at_ms: 0,
                    }),
                    Ok(None) => unverifiable.push(format!(
                        "{}: file `{path}` not found under the workspace",
                        crit.id
                    )),
                    Err(reason) => unverifiable.push(format!("{}: {reason}", crit.id)),
                }
            }
            Verifier::CommandExit { cmd, code } if *code == 0 => {
                let mut parts = cmd.split_whitespace();
                match parts.next() {
                    Some(prog) => command_validators.push(mechanical(
                        &crit.id,
                        ValidatorSpec::Command {
                            cmd: prog.to_string(),
                            args: parts.map(str::to_string).collect(),
                        },
                    )),
                    None => unverifiable.push(format!("{}: empty CommandExit command", crit.id)),
                }
            }
            other => unverifiable.push(format!(
                "{}: unsupported verifier {other:?} (v1 executor)",
                crit.id
            )),
        }
    }

    // (a) Any unverifiable/failed criterion fails the gate closed — an executor
    //     that cannot check "done" must not certify it done.
    if !unverifiable.is_empty() {
        let reason = format!(
            "acceptance cannot be verified — {}",
            unverifiable.join("; ")
        );
        tracing::warn!(
            %reason,
            "fleet worker: acceptance gate fails closed on unsupported/failed criteria",
        );
        return (
            AcceptanceVerdict::Rejected {
                reason: reason.clone(),
            },
            Some(reason),
        );
    }

    // No `CommandExit` validators — any `FileExists` criteria already passed
    // in-worker, so the (possibly empty) evidence set IS the gate.
    if command_validators.is_empty() {
        return (AcceptanceVerdict::Accepted { evidence }, None);
    }

    // (P1-5c) Command validators under the SHARED sandbox. (round-4 P1) Drive
    // them ONE AT A TIME with a SHRINKING remaining budget: each validator's
    // own `timeout_ms` is set to the remaining deadline, so the validator's
    // cancellation-safe internal `timeout` + `kill_child_process` actually
    // group-kills the subprocess AT the deadline. Dropping an outer
    // `tokio::time::timeout` future does NOT kill a Tokio child, so relying on
    // it alone would leak a `sleep`ing subprocess past the recorded terminal
    // state. Subtracting each run's elapsed keeps the TOTAL phase bounded by
    // the initial `remaining`.
    // The acceptance gate runs `CommandExit` validators through the SAME granted
    // registry the agent held (so the shell is configured identically). An
    // incoherent grant would already have failed the agent build above; handle
    // the Result defensively.
    let registry = match factory.build_registry_with(
        workspace_root,
        sandbox.clone(),
        max_shell_timeout_secs,
        &task_view.grant,
    ) {
        Ok(registry) => registry,
        Err(err) => {
            let reason = format!("invalid worker grant for acceptance: {err}");
            return (
                AcceptanceVerdict::Terminated {
                    reason: reason.clone(),
                },
                Some(reason),
            );
        }
    };
    let runner = ValidatorRunner::new(Arc::new(registry), workspace_root.to_path_buf())
        .with_sandbox(sandbox);
    let invocation = ValidatorInvocation::new(
        ValidatorPhase::Completion,
        workspace_root.to_path_buf(),
        "fleet-worker".to_string(),
    );

    let deadline_terminated = || {
        let reason = "acceptance deadline exceeded".to_string();
        (
            AcceptanceVerdict::Terminated {
                reason: reason.clone(),
            },
            Some(reason),
        )
    };

    let mut outcomes: Vec<ValidatorOutcome> = Vec::with_capacity(command_validators.len());
    for mut validator in command_validators {
        if remaining.is_zero() {
            // Earlier validators consumed the whole acceptance budget — fail
            // closed on the deadline rather than run one with a 0ms timeout.
            return deadline_terminated();
        }
        // Bound THIS validator by the remaining budget so its OWN timeout+kill
        // fire at the deadline (min with any pre-configured timeout).
        let budget_ms = u64::try_from(remaining.as_millis())
            .unwrap_or(u64::MAX)
            .max(1);
        validator.timeout_ms = Some(match validator.timeout_ms {
            Some(configured) => configured.min(budget_ms),
            None => budget_ms,
        });

        let started = Instant::now();
        // Coarse outer backstop ONLY (grace beyond the per-validator budget so
        // the validator's own internal timeout+kill wins the race and returns a
        // `Timeout` outcome). It guards a validator hanging outside that awaited
        // kill path — never the killing itself.
        let slice = std::slice::from_ref(&validator);
        let batch = tokio::time::timeout(
            remaining + ACCEPTANCE_BACKSTOP_GRACE,
            runner.run_all(&invocation, slice),
        )
        .await;
        remaining = remaining.saturating_sub(started.elapsed());
        match batch {
            Ok(mut got) => outcomes.append(&mut got),
            // The backstop fired (a validator hung past its own killing budget):
            // we can't prove the subprocess died, so fail closed on the deadline.
            Err(_elapsed) => return deadline_terminated(),
        }
    }

    // A validator group-killed at its deadline reports `Timeout` — that is a
    // deadline overrun (Terminated), distinct from a clean `Fail` (Rejected).
    if outcomes
        .iter()
        .any(|o| o.status == ValidatorStatus::Timeout)
    {
        return deadline_terminated();
    }

    let passed = outcomes.iter().all(|o| o.required_gate_passed());

    if passed {
        evidence.extend(outcomes.iter().map(|o| {
            EvidenceRef {
                kind: o.kind.clone(),
                locator: o
                    .evidence_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| o.validator_id.clone()),
                sha256: String::new(),
                captured_at_ms: o.started_at.timestamp_millis().max(0) as u64,
            }
        }));
        (AcceptanceVerdict::Accepted { evidence }, None)
    } else {
        let failed: Vec<String> = outcomes
            .iter()
            .filter(|o| !o.required_gate_passed())
            .map(|o| format!("{}: {}", o.validator_id, o.reason))
            .collect();
        let reason = format!("acceptance failed — {}", failed.join("; "));
        (
            AcceptanceVerdict::Rejected {
                reason: reason.clone(),
            },
            Some(reason),
        )
    }
}

/// Evaluate a `FileExists` criterion IN THE WORKER, resistant to symlink
/// escape (P1-5a). Confines `path` lexically (via [`confined_relative`]), then
/// resolves it under `workspace_root` following symlinks (`canonicalize`) and
/// asserts the canonical target stays UNDER the canonical workspace root and is
/// a regular file.
///
/// Returns `Ok(Some(canonical))` when the file exists and is confined,
/// `Ok(None)` when it is simply absent (fail-closed "not found"), and `Err`
/// when it exists but escapes the workspace or isn't a regular file.
///
/// **Point-in-time only (TOCTOU, P2).** The containment `canonicalize` and the
/// follow-up `metadata` are distinct syscalls, so a concurrent writer that
/// swaps an in-workspace file for an external symlink between them can have
/// `metadata` follow the symlink. This is sound ONLY under v1's
/// single-writer-per-task-workspace assumption (see the module "Known v1
/// limitations"); it is a point-in-time confinement, not a race-proof absolute
/// guarantee against a hostile concurrent mutator of the task's own workspace.
fn check_file_exists_confined(
    workspace_root: &Path,
    path: &str,
) -> Result<Option<PathBuf>, String> {
    confined_relative(path)?;
    let target = workspace_root.join(path);
    // `canonicalize` follows symlinks and requires existence; a missing target
    // (or a dangling symlink) is "not found", not an escape.
    let canonical = match std::fs::canonicalize(&target) {
        Ok(p) => p,
        Err(_) => return Ok(None),
    };
    let canonical_root = std::fs::canonicalize(workspace_root)
        .map_err(|e| format!("cannot canonicalize workspace root: {e}"))?;
    if !canonical.starts_with(&canonical_root) {
        return Err(format!(
            "file `{path}` resolves OUTSIDE the workspace (symlink escape)"
        ));
    }
    match std::fs::metadata(&canonical) {
        Ok(m) if m.is_file() => Ok(Some(canonical)),
        Ok(_) => Err(format!("`{path}` exists but is not a regular file")),
        Err(e) => Err(format!("cannot stat `{path}`: {e}")),
    }
}

/// Reject a `FileExists` acceptance path that isn't lexically confined to the
/// task workspace. An absolute path (`/etc/passwd`) or one with a `..`
/// component (`../../etc/passwd`) could assert outside the worker's cwd. This
/// is the LEXICAL half; [`check_file_exists_confined`] adds symlink-resolved
/// confinement on top.
fn confined_relative(path: &str) -> Result<(), String> {
    use std::path::Component;
    let p = Path::new(path);
    if p.is_absolute() {
        return Err(format!(
            "FileExists path `{path}` must be workspace-relative, not absolute"
        ));
    }
    for comp in p.components() {
        match comp {
            Component::ParentDir => {
                return Err(format!(
                    "FileExists path `{path}` must not escape the workspace with `..`"
                ));
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err(format!(
                    "FileExists path `{path}` must be workspace-relative"
                ));
            }
            Component::Normal(_) | Component::CurDir => {}
        }
    }
    Ok(())
}

/// A hard-required completion-phase validator for `spec`.
fn mechanical(id: &str, spec: ValidatorSpec) -> Validator {
    Validator {
        id: id.to_string(),
        required: true,
        soft_fail: false,
        timeout_ms: None,
        phase: ValidatorPhaseKind::Completion,
        spec,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use octos_agent::sandbox::NoSandbox;
    use octos_fleet::{AttemptStatus, ChildStatus};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tempfile::TempDir;

    #[test]
    fn file_exists_confined_rejects_symlink_escape() {
        // P1-5a: a real in-workspace file passes; an absent file is
        // fail-closed "not found"; an in-workspace symlink pointing OUTSIDE
        // the workspace is rejected (the escape the lexical check misses).
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        std::fs::write(ws.join("real.txt"), b"x").unwrap();
        assert!(matches!(
            check_file_exists_confined(ws, "real.txt"),
            Ok(Some(_))
        ));
        assert!(matches!(
            check_file_exists_confined(ws, "nope.txt"),
            Ok(None)
        ));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/passwd", ws.join("escape.txt")).unwrap();
            assert!(
                check_file_exists_confined(ws, "escape.txt").is_err(),
                "in-workspace symlink escaping the workspace must be rejected",
            );
        }
        // Lexical escapes are still rejected up front.
        assert!(check_file_exists_confined(ws, "/etc/passwd").is_err());
        assert!(check_file_exists_confined(ws, "../escape").is_err());
    }

    #[test]
    fn confined_relative_accepts_workspace_paths_and_rejects_escapes() {
        // Confined, workspace-relative paths pass.
        assert!(confined_relative("out.txt").is_ok());
        assert!(confined_relative("sub/dir/out.txt").is_ok());
        assert!(confined_relative("./out.txt").is_ok());
        // Absolute + `..`-escaping paths are rejected.
        assert!(confined_relative("/etc/passwd").is_err());
        assert!(confined_relative("../escape.txt").is_err());
        assert!(confined_relative("sub/../../escape.txt").is_err());
    }

    async fn view_of(fleet: &Fleet, task_id: &str) -> TaskView {
        fleet
            .view()
            .await
            .unwrap()
            .tasks
            .into_iter()
            .find(|t| t.task_id == task_id)
            .expect("task in view")
    }

    #[tokio::test]
    async fn grant_network_threads_allow_network_into_sandbox() {
        // PR A: `run_attempt` derives the sandbox's `allow_network` from the
        // task's grant — a `Full` grant threads `true`, a minimal grant `false`.
        // A recording sandbox factory captures the flag it is handed.
        use std::sync::Mutex;
        let seen: Arc<Mutex<Vec<bool>>> = Arc::new(Mutex::new(Vec::new()));

        async fn run_with_grant(
            seen: Arc<std::sync::Mutex<Vec<bool>>>,
            grant: octos_fleet::WorkerGrant,
        ) {
            let (_sd, store) = fresh_store().await;
            let fleet = create_fleet(
                store.clone(),
                "f1",
                vec![task_spec_granted("a", &[], vec![], grant)],
            )
            .await;
            let attempt = launch(&store, "f1", "a").await;

            let (_md, memory) = fresh_memory().await;
            let rec = seen.clone();
            let factory = AgentFactory::new(
                Arc::new(SuccessProvider),
                memory,
                Arc::new(move |_cwd, allow_network| {
                    rec.lock().unwrap().push(allow_network);
                    Arc::new(MarkerSandbox) as Arc<dyn Sandbox>
                }),
            );
            let task_view = view_of(&fleet, "a").await;
            let work = TempDir::new().unwrap();
            let _ = run_attempt(
                store.clone(),
                "f1",
                "a",
                &attempt,
                &task_view,
                &factory,
                work.path(),
                Duration::from_secs(30),
                EPOCH,
                || NOW,
            )
            .await;
        }

        // Minimal grant → no raw egress.
        run_with_grant(seen.clone(), octos_fleet::WorkerGrant::minimal()).await;
        // Full grant → raw egress.
        run_with_grant(
            seen.clone(),
            octos_fleet::WorkerGrant {
                network: octos_fleet::NetworkGrant::Full,
                ..octos_fleet::WorkerGrant::minimal()
            },
        )
        .await;

        let flags = seen.lock().unwrap().clone();
        assert!(
            flags.contains(&false),
            "a minimal grant must build the sandbox with allow_network=false: {flags:?}",
        );
        assert!(
            flags.contains(&true),
            "a Full grant must build the sandbox with allow_network=true: {flags:?}",
        );
    }

    #[tokio::test]
    async fn run_attempt_happy_path() {
        let (_sd, store) = fresh_store().await;
        let fleet = create_fleet(
            store.clone(),
            "f1",
            vec![task_spec("a", &[], file_exists("out.txt"))],
        )
        .await;
        let attempt = launch(&store, "f1", "a").await;

        let work = TempDir::new().unwrap();
        let (_md, factory) = factory_for(Arc::new(WriteFileProvider::new("out.txt"))).await;
        let task_view = view_of(&fleet, "a").await;

        let outcome = run_attempt(
            store.clone(),
            "f1",
            "a",
            &attempt,
            &task_view,
            &factory,
            work.path(),
            Duration::from_secs(30),
            EPOCH,
            || NOW,
        )
        .await;

        assert!(
            matches!(
                outcome,
                AttemptOutcome::Completed {
                    verdict: AcceptanceVerdict::Accepted { .. }
                }
            ),
            "expected Completed/Accepted, got {outcome:?}",
        );

        // The artifact really landed under the worker cwd.
        assert!(
            work.path().join("out.txt").exists(),
            "acceptance file missing"
        );

        // Child terminal Succeeded; snapshot carries the real result; budget advanced.
        let child = store.get_child("f1", "a").await.unwrap().unwrap();
        assert_eq!(child.status, ChildStatus::Succeeded);
        let att = store.get_attempt("a", &attempt).await.unwrap().unwrap();
        assert_eq!(att.status, AttemptStatus::Done);
        let snap = att.result_snapshot.expect("snapshot recorded");
        assert!(snap.success, "snapshot must record success");
        assert!(snap.tokens_used > 0, "tokens must be recorded");
        let fleet_rec = store.get_fleet("f1").await.unwrap().unwrap();
        assert!(
            fleet_rec.budget.tokens_committed > 0,
            "fleet.tokens_committed must advance",
        );
        assert_eq!(fleet_rec.budget.tokens_reserved, 0, "reservation released");
    }

    #[tokio::test]
    async fn attempt_creates_one_shared_sandbox_instance() {
        // P1-3-fix: the sandbox factory is invoked EXACTLY once per attempt —
        // the single instance is shared by the agent registry and the
        // acceptance validators. (Round-1 re-invoked the factory for the
        // validator registry, so a non-idempotent factory could sandbox the
        // agent yet hand the validators a weaker sandbox.)
        let (_sd, store) = fresh_store().await;
        let fleet = create_fleet(
            store.clone(),
            "f1",
            vec![task_spec("a", &[], command_exit("true"))],
        )
        .await;
        let attempt = launch(&store, "f1", "a").await;

        let work = TempDir::new().unwrap();
        let (_md, memory) = fresh_memory().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let factory = AgentFactory::new(
            Arc::new(SuccessProvider),
            memory,
            // MarkerSandbox (isolating test double) so the attempt-time
            // fail-closed guard (H1) lets the agent run; still counts factory
            // invocations to prove the single-shared-instance contract.
            Arc::new(move |_, _| {
                seen.fetch_add(1, Ordering::SeqCst);
                Arc::new(MarkerSandbox) as Arc<dyn Sandbox>
            }),
        );
        let task_view = view_of(&fleet, "a").await;

        let outcome = run_attempt(
            store.clone(),
            "f1",
            "a",
            &attempt,
            &task_view,
            &factory,
            work.path(),
            Duration::from_secs(30),
            EPOCH,
            || NOW,
        )
        .await;
        assert!(
            matches!(
                outcome,
                AttemptOutcome::Completed {
                    verdict: AcceptanceVerdict::Accepted { .. }
                }
            ),
            "CommandExit `true` must be accepted, got {outcome:?}",
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the sandbox factory must be invoked exactly once per attempt (shared instance)",
        );
    }

    /// #1857 PR 5a fix (H1, codex round 2) — ATTEMPT-TIME fail-closed: the boot
    /// probe checks ONE sandbox, but the factory rebuilds the sandbox per attempt
    /// and `Auto` can fall through to `NoSandbox` if the backend vanished AFTER
    /// boot. A no-op sandbox at attempt time must TERMINATE the attempt WITHOUT
    /// building or running the agent — never run a fleet worker unsandboxed.
    #[tokio::test]
    async fn run_attempt_terminates_when_sandbox_is_not_isolating() {
        let (_sd, store) = fresh_store().await;
        let fleet = create_fleet(
            store.clone(),
            "f1",
            vec![task_spec("a", &[], file_exists("out.txt"))],
        )
        .await;
        let attempt = launch(&store, "f1", "a").await;

        let work = TempDir::new().unwrap();
        let (_md, memory) = fresh_memory().await;
        // A genuine no-op sandbox + a provider that MUST NOT be called: proving
        // the attempt aborts BEFORE the agent (and thus any LLM call) is built.
        let calls = Arc::new(AtomicUsize::new(0));
        let factory = AgentFactory::new(
            Arc::new(CountingProvider {
                calls: calls.clone(),
            }),
            memory,
            Arc::new(|_, _| Arc::new(NoSandbox) as Arc<dyn Sandbox>),
        );
        let task_view = view_of(&fleet, "a").await;

        let outcome = run_attempt(
            store.clone(),
            "f1",
            "a",
            &attempt,
            &task_view,
            &factory,
            work.path(),
            Duration::from_secs(30),
            EPOCH,
            || NOW,
        )
        .await;

        assert!(
            matches!(
                outcome,
                AttemptOutcome::Completed {
                    verdict: AcceptanceVerdict::Terminated { .. }
                }
            ),
            "a no-op sandbox must Terminate the attempt, got {outcome:?}",
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "the agent/LLM must NEVER run under a no-op sandbox",
        );
        // Recorded via the normal completion path: the child ends terminal
        // (Failed), not silently left running unsandboxed.
        let child = store.get_child("f1", "a").await.unwrap().unwrap();
        assert_eq!(child.status, ChildStatus::Failed);
    }

    #[tokio::test]
    async fn acceptance_command_terminates_past_deadline() {
        // P1-5b: a CommandExit validator that sleeps past the remaining
        // deadline makes the attempt end Terminated (child Failed), not hang.
        let (_sd, store) = fresh_store().await;
        let fleet = create_fleet(
            store.clone(),
            "f1",
            vec![task_spec("a", &[], command_exit("sleep 20"))],
        )
        .await;
        let attempt = launch(&store, "f1", "a").await;

        let work = TempDir::new().unwrap();
        // SuccessProvider finishes instantly, so acceptance gets ~the whole
        // 2s deadline — which the `sleep 20` command overruns.
        let (_md, factory) = factory_for(Arc::new(SuccessProvider)).await;
        let task_view = view_of(&fleet, "a").await;

        let started = std::time::Instant::now();
        let outcome = run_attempt(
            store.clone(),
            "f1",
            "a",
            &attempt,
            &task_view,
            &factory,
            work.path(),
            Duration::from_secs(2),
            EPOCH,
            || NOW,
        )
        .await;
        assert!(
            matches!(
                outcome,
                AttemptOutcome::Completed {
                    verdict: AcceptanceVerdict::Terminated { .. }
                }
            ),
            "acceptance overrun must Terminate, got {outcome:?}",
        );
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "must not wait for the full 20s command sleep",
        );
        let child = store.get_child("f1", "a").await.unwrap().unwrap();
        assert_eq!(child.status, ChildStatus::Failed);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn acceptance_command_kills_subprocess_past_deadline() {
        // P1 (round-4): the acceptance timeout must KILL the validator
        // subprocess, not merely drop the outer future — Tokio does NOT kill a
        // child on drop. A CommandExit that sleeps PAST the deadline and THEN
        // writes a marker must be group-killed at the deadline (so the marker is
        // NEVER written), rather than leaking a live `sleep` that mutates the
        // (reused) task workspace after the recorded terminal state.
        use std::os::unix::fs::PermissionsExt;

        let (_sd, store) = fresh_store().await;

        // Held tempdir so both the script path and the marker outlive the run.
        // The script sleeps 4s, THEN writes the marker; its own kill (fired by
        // the validator's per-run timeout) must preempt that write.
        let scratch = TempDir::new().unwrap();
        let script = scratch.path().join("killme.sh");
        let marker = scratch.path().join("marker.done");
        std::fs::write(
            &script,
            format!("#!/bin/sh\nsleep 4\n: > {}\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let fleet = create_fleet(
            store.clone(),
            "f1",
            vec![task_spec("a", &[], command_exit(&script.to_string_lossy()))],
        )
        .await;
        let attempt = launch(&store, "f1", "a").await;

        let work = TempDir::new().unwrap();
        // SuccessProvider finishes instantly, so acceptance gets ~the whole 2s
        // deadline — which the `sleep 4` command overruns and is killed at.
        let (_md, factory) = factory_for(Arc::new(SuccessProvider)).await;
        let task_view = view_of(&fleet, "a").await;

        let outcome = run_attempt(
            store.clone(),
            "f1",
            "a",
            &attempt,
            &task_view,
            &factory,
            work.path(),
            Duration::from_secs(2),
            EPOCH,
            || NOW,
        )
        .await;
        assert!(
            matches!(
                outcome,
                AttemptOutcome::Completed {
                    verdict: AcceptanceVerdict::Terminated { .. }
                }
            ),
            "acceptance overrun must Terminate, got {outcome:?}",
        );

        // Wait WELL PAST the script's 4s sleep: a leaked (un-killed) subprocess
        // would have written the marker by now; a group-killed one never does.
        tokio::time::sleep(Duration::from_secs(4)).await;
        assert!(
            !marker.exists(),
            "validator subprocess was NOT killed — it survived the deadline and \
             wrote its marker (dropping the outer future does not kill a Tokio child)",
        );
    }

    #[tokio::test]
    async fn run_attempt_rejects_when_acceptance_fails() {
        let (_sd, store) = fresh_store().await;
        let fleet = create_fleet(
            store.clone(),
            "f1",
            vec![task_spec("a", &[], file_exists("out.txt"))],
        )
        .await;
        let attempt = launch(&store, "f1", "a").await;

        let work = TempDir::new().unwrap();
        // SuccessProvider ends the turn WITHOUT writing out.txt, so the run
        // succeeds but the FileExists gate fails.
        let (_md, factory) = factory_for(Arc::new(SuccessProvider)).await;
        let task_view = view_of(&fleet, "a").await;

        let outcome = run_attempt(
            store.clone(),
            "f1",
            "a",
            &attempt,
            &task_view,
            &factory,
            work.path(),
            Duration::from_secs(30),
            EPOCH,
            || NOW,
        )
        .await;

        assert!(
            matches!(
                outcome,
                AttemptOutcome::Completed {
                    verdict: AcceptanceVerdict::Rejected { .. }
                }
            ),
            "expected Completed/Rejected, got {outcome:?}",
        );
        let child = store.get_child("f1", "a").await.unwrap().unwrap();
        assert_eq!(child.status, ChildStatus::Failed);
        let att = store.get_attempt("a", &attempt).await.unwrap().unwrap();
        let snap = att.result_snapshot.expect("snapshot recorded");
        assert!(!snap.success, "rejected snapshot must record failure");
        assert!(snap.error.is_some(), "rejection reason must be recorded");
    }

    #[tokio::test]
    async fn run_attempt_rejects_unsupported_validator_ref() {
        // P1-5a: a `ValidatorRef`-only task must NOT pass on the run's own
        // self-report — the executor cannot verify it, so the gate fails
        // closed (Rejected), even though the run succeeded.
        let (_sd, store) = fresh_store().await;
        let fleet = create_fleet(
            store.clone(),
            "f1",
            vec![task_spec("a", &[], validator_ref("tests"))],
        )
        .await;
        let attempt = launch(&store, "f1", "a").await;

        let work = TempDir::new().unwrap();
        let (_md, factory) = factory_for(Arc::new(SuccessProvider)).await;
        let task_view = view_of(&fleet, "a").await;

        let outcome = run_attempt(
            store.clone(),
            "f1",
            "a",
            &attempt,
            &task_view,
            &factory,
            work.path(),
            Duration::from_secs(30),
            EPOCH,
            || NOW,
        )
        .await;

        assert!(
            matches!(
                outcome,
                AttemptOutcome::Completed {
                    verdict: AcceptanceVerdict::Rejected { .. }
                }
            ),
            "unsupported ValidatorRef must fail closed, got {outcome:?}",
        );
        let child = store.get_child("f1", "a").await.unwrap().unwrap();
        assert_eq!(child.status, ChildStatus::Failed);
    }

    #[tokio::test]
    async fn run_attempt_rejects_out_of_workspace_file_exists() {
        // P1-5b: a `FileExists` on an ABSOLUTE path (e.g. `/etc/passwd`, which
        // really exists on the host) must NOT be accepted — the acceptance
        // gate is confined to the task workspace, so it fails closed.
        let (_sd, store) = fresh_store().await;
        let fleet = create_fleet(
            store.clone(),
            "f1",
            vec![task_spec("a", &[], file_exists("/etc/passwd"))],
        )
        .await;
        let attempt = launch(&store, "f1", "a").await;

        let work = TempDir::new().unwrap();
        let (_md, factory) = factory_for(Arc::new(SuccessProvider)).await;
        let task_view = view_of(&fleet, "a").await;

        let outcome = run_attempt(
            store.clone(),
            "f1",
            "a",
            &attempt,
            &task_view,
            &factory,
            work.path(),
            Duration::from_secs(30),
            EPOCH,
            || NOW,
        )
        .await;

        assert!(
            !matches!(
                outcome,
                AttemptOutcome::Completed {
                    verdict: AcceptanceVerdict::Accepted { .. }
                }
            ),
            "an absolute-path FileExists must not be accepted, got {outcome:?}",
        );
        let child = store.get_child("f1", "a").await.unwrap().unwrap();
        assert_eq!(child.status, ChildStatus::Failed);
    }

    #[tokio::test]
    async fn run_attempt_terminates_on_deadline() {
        let (_sd, store) = fresh_store().await;
        let fleet = create_fleet(store.clone(), "f1", vec![task_spec("a", &[], vec![])]).await;
        let attempt = launch(&store, "f1", "a").await;

        let work = TempDir::new().unwrap();
        // Sleeps 30s; the 200ms deadline must fire first.
        let (_md, factory) = factory_for(Arc::new(SleepProvider {
            hold: Duration::from_secs(30),
        }))
        .await;
        let task_view = view_of(&fleet, "a").await;

        let outcome = run_attempt(
            store.clone(),
            "f1",
            "a",
            &attempt,
            &task_view,
            &factory,
            work.path(),
            Duration::from_millis(200),
            EPOCH,
            || NOW,
        )
        .await;

        assert!(
            matches!(
                outcome,
                AttemptOutcome::Completed {
                    verdict: AcceptanceVerdict::Terminated { .. }
                }
            ),
            "expected Completed/Terminated, got {outcome:?}",
        );
        let child = store.get_child("f1", "a").await.unwrap().unwrap();
        assert_eq!(child.status, ChildStatus::Failed);
        let att = store.get_attempt("a", &attempt).await.unwrap().unwrap();
        let snap = att.result_snapshot.expect("snapshot recorded");
        assert!(!snap.success);
        assert!(
            snap.error.unwrap().contains("deadline"),
            "error must name the deadline",
        );
    }

    #[tokio::test]
    async fn run_attempt_aborts_on_lost_mark_running() {
        let (_sd, store) = fresh_store().await;
        let fleet = create_fleet(store.clone(), "f1", vec![task_spec("a", &[], vec![])]).await;
        let attempt = launch(&store, "f1", "a").await;
        // Externally advance the child to Running so run_attempt's own
        // mark_running (which requires child Launching) loses the race.
        store.mark_running("a", &attempt).await.unwrap();

        let work = TempDir::new().unwrap();
        let (_md, factory) = factory_for(Arc::new(SuccessProvider)).await;
        let task_view = view_of(&fleet, "a").await;

        let outcome = run_attempt(
            store.clone(),
            "f1",
            "a",
            &attempt,
            &task_view,
            &factory,
            work.path(),
            Duration::from_secs(30),
            EPOCH,
            || NOW,
        )
        .await;

        assert!(
            matches!(outcome, AttemptOutcome::Aborted { .. }),
            "got {outcome:?}"
        );
        // It must NOT have completed: child still Running, attempt still
        // Running with no snapshot.
        let child = store.get_child("f1", "a").await.unwrap().unwrap();
        assert_eq!(child.status, ChildStatus::Running);
        let att = store.get_attempt("a", &attempt).await.unwrap().unwrap();
        assert_eq!(att.status, AttemptStatus::Running);
        assert!(
            att.result_snapshot.is_none(),
            "aborted attempt must not record a snapshot"
        );
    }

    #[tokio::test]
    async fn two_dep_chain_promotes_successor_after_completion() {
        let (_sd, store) = fresh_store().await;
        let fleet = create_fleet(
            store.clone(),
            "f1",
            vec![task_spec("a", &[], vec![]), task_spec("b", &["a"], vec![])],
        )
        .await;
        // b depends on a, so it starts Planned.
        assert_eq!(
            store.get_child("f1", "b").await.unwrap().unwrap().status,
            ChildStatus::Planned,
        );

        let attempt = launch(&store, "f1", "a").await;
        let work = TempDir::new().unwrap();
        let (_md, factory) = factory_for(Arc::new(SuccessProvider)).await;
        let task_view = view_of(&fleet, "a").await;

        let outcome = run_attempt(
            store.clone(),
            "f1",
            "a",
            &attempt,
            &task_view,
            &factory,
            work.path(),
            Duration::from_secs(30),
            EPOCH,
            || NOW,
        )
        .await;
        assert!(
            matches!(
                outcome,
                AttemptOutcome::Completed {
                    verdict: AcceptanceVerdict::Accepted { .. }
                }
            ),
            "got {outcome:?}",
        );

        // a Succeeded and, via the post-completion ready_tasks, b is promoted
        // Ready and now launchable.
        assert_eq!(
            store.get_child("f1", "a").await.unwrap().unwrap().status,
            ChildStatus::Succeeded,
        );
        assert_eq!(
            store.get_child("f1", "b").await.unwrap().unwrap().status,
            ChildStatus::Ready,
        );
        let b_attempt = launch(&store, "f1", "b").await;
        assert!(!b_attempt.is_empty(), "successor must be launchable");
    }
}
