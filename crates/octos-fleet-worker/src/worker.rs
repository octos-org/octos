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
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use octos_agent::TokenTracker;
use octos_agent::sandbox::Sandbox;
use octos_agent::validators::{
    ValidatorInvocation, ValidatorOutcome, ValidatorPhase, ValidatorRunner, ValidatorStatus,
};
use octos_agent::workspace_policy::{Validator, ValidatorPhaseKind, ValidatorSpec};
use octos_core::{Message, Task, TaskContext, TaskKind, TaskResult, TokenUsage};
use octos_fleet::{
    AcceptanceVerdict, ChildResultSnapshot, CompleteOutcome, EscalationRequest, EvidenceRef, Fleet,
    FleetKernelStore, MarkRunningOutcome, TaskView, Verifier,
};

use crate::escalate::EscalationSlot;
use crate::{AgentFactory, SandboxGrant};

/// The controller-repo context for a WORKTREE attempt, threaded into
/// [`run_attempt`] so it can land the deliverable on the task branch before
/// completion and refuse to record a phantom success on an EMPTY branch. `None`
/// for the scratch fallback (no worktree, no branch).
#[derive(Debug, Clone)]
pub struct WorktreeContext {
    /// Canonical controller repository root.
    pub repo_root: PathBuf,
    /// The task-stable `fleet/<fleet>/<task>` branch the checkout is on.
    pub branch: String,
    /// The branch head captured at prepare — the base the deliverable must
    /// advance past for the attempt to count as having produced work.
    pub base_commit: String,
    /// The repository's `.git` common dir (objects/refs/worktree-admin), OUTSIDE
    /// the checkout cwd. Threaded into the sandbox grant (`repo_git_dir`) so the
    /// worker's `git commit` can rw the ONLY path beyond its cwd it needs — a
    /// TARGETED bind, not full-`/`.
    pub git_dir: PathBuf,
}

/// Settle an escalation NON-terminally via the store's four-part fence, mapping
/// the store outcome to an [`AttemptOutcome`]. Shared by `run_attempt` (the
/// normal path) so the mapping lives in one place.
#[allow(clippy::too_many_arguments)] // store + ids + request + settlement inputs are irreducible here
async fn settle_escalation(
    store: &FleetKernelStore,
    fleet_id: &str,
    task_id: &str,
    attempt_id: &str,
    request: EscalationRequest,
    actual_tokens: u64,
    owner_epoch: u64,
    now_ms: u64,
) -> AttemptOutcome {
    match store
        .record_escalation(
            fleet_id,
            task_id,
            attempt_id,
            request.clone(),
            actual_tokens,
            owner_epoch,
            now_ms,
        )
        .await
    {
        Ok(CompleteOutcome::Completed) => {
            tracing::info!(
                %fleet_id, %task_id, %attempt_id, tokens = actual_tokens,
                "fleet worker: attempt escalated — child Blocked pending an operator grant decision",
            );
            AttemptOutcome::Escalated { request }
        }
        // A stale/superseded attempt lost the fence: NOT ours (relaunch/recovery
        // owns it), so the pool disarms.
        Ok(CompleteOutcome::Superseded) => AttemptOutcome::Superseded,
        Err(err) => {
            tracing::error!(
                %fleet_id, %task_id, %attempt_id, error = %err,
                "fleet worker: record_escalation errored",
            );
            AttemptOutcome::RecordError {
                reason: err.to_string(),
            }
        }
    }
}

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
    /// PR B — the attempt yielded on a mid-task ESCALATION: it recorded a
    /// request for a wider grant and `record_escalation` settled it
    /// NON-terminally (child `Blocked` + `pending_escalation`, the yielded
    /// attempt's REAL tokens committed). The keeper's `goal_grant`/`goal_deny`
    /// decides what happens next. This is a SETTLED terminal disposition for the
    /// attempt, so the pool disarms the guard (no double-settle) — the child is
    /// simply parked on the operator, not wedged in `Launching`.
    Escalated { request: EscalationRequest },
}

/// Run one launched attempt to a recorded terminal state. See the module
/// docs for the sequence. `actual_now_ms` is the wall clock threaded into
/// the store's `complete_child` (settlement timestamp) and the follow-on
/// readiness promotion.
#[allow(clippy::too_many_arguments)] // store + ids + view + factory + cwd + worktree + deadline + epoch + clock + escalation slot are irreducible
pub async fn run_attempt(
    store: Arc<FleetKernelStore>,
    fleet_id: &str,
    task_id: &str,
    attempt_id: &str,
    task_view: &TaskView,
    factory: &AgentFactory,
    working_dir: &Path,
    // Some(ctx) when this attempt runs in a git worktree (the pool allocated one
    // for a `FsGrant::Host` task on a git controller root under a supporting
    // backend); None for the scratch-cwd fallback. Drives the deliverable
    // auto-commit + empty-branch reject.
    worktree: Option<&WorktreeContext>,
    deadline: Duration,
    owner_epoch: u64,
    // PR B — the shared escalation slot the always-on `escalate` valve writes
    // into. Created by the pool BEFORE arming the LaunchGuard and passed to BOTH,
    // so a cancel that drops this run still settles escalated (not `Terminated`).
    escalation_slot: EscalationSlot,
    // PR B — the shared token tracker the agent folds each response's cumulative
    // spend into (via `run_task_with_tracker`). Created by the pool alongside the
    // escalation slot and shared into the LaunchGuard, so a cancel-drop escalation
    // settle reads the REAL spend from the SAME tracker this run updated — never 0.
    tracker: Arc<TokenTracker>,
    // PR B (codex round-4, defect 1) — the tokens this attempt RESERVED at launch
    // (the pool's `projected_tokens`, == the attempt's `reserved_tokens`), threaded
    // in reliably as the never-zero escalation floor so the settle never depends on
    // a fallible re-fetch. See [`escalation_tokens_with_floor`].
    reserved_tokens: u64,
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

    // 2. Build the Task from the rendered brief + acceptance criteria. For a
    //    worktree attempt, add the deliverable hint (belt-and-suspenders on the
    //    auto-commit): the worker's deliverable is a COMMIT on its branch.
    let mut brief = render_brief(task_view);
    if worktree.is_some() {
        brief.push_str(
            "\n## Deliverable\nYou are working inside a `git worktree` on your own branch. Your \
             deliverable is a COMMIT on that branch: make your changes here, then `git add -A` and \
             `git commit`. (Any changes you leave uncommitted are auto-committed on completion, but \
             an empty branch with no changes counts as NO deliverable.)\n",
        );
    }
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
            working_memory: vec![Message::user(brief)],
            ..Default::default()
        },
    );

    // The sandbox scope comes from THIS task's grant (projected fresh per
    // attempt), not a hardcode:
    // - `allow_network` (PR A): `None`/`Hosts` → no raw egress (the shell cannot
    //   `curl`; `Hosts` is enforced by the granted web tools); `Full` → raw
    //   egress (git/npm/etc.).
    // - `repo_git_dir` (PR C, codex fix #2a): set ONLY for an actual WORKTREE
    //   worker (to the prepared checkout's `<repo>/.git`), NOT from `fs.is_host()`
    //   alone. The pool allocates a worktree only when the FULL gate holds —
    //   `FsGrant::Host` AND `NetworkGrant::Full` (a coherent full-trust grant) AND
    //   a git repo AND a supporting backend — so a SCRATCH-fallback worker
    //   (restricted grant, non-git, or unsupported backend) stays cwd-confined and
    //   never gets the extra `.git` write. It is a TARGETED bind, never full-`/`,
    //   so no host AF_UNIX socket is exposed above the worker's grant.
    let grant = SandboxGrant {
        allow_network: task_view.grant.network.allows_raw_egress(),
        repo_git_dir: worktree.map(|w| w.git_dir.clone()),
        // #1976 — project the per-path write fence into the sandbox scope so
        // the SHELL is bounded to the same allowlist as the file tools (macOS
        // OS-enforced; other backends degrade to read-only workspace). A
        // fence and a worktree (`FsGrant::Host`) are mutually exclusive by
        // construction — `validate()` rejects `write_paths` under `Host`, and
        // the pool only allocates a worktree for a full-trust Host grant — so
        // `write_allow_globs` and `repo_git_dir` are never both `Some`.
        write_allow_globs: task_view.grant.write_paths.clone(),
    };

    // P1-3-fix: ONE sandbox instance for the whole attempt, shared by the
    // agent's granted registry AND the acceptance validators — so a
    // non-idempotent factory can never sandbox the agent while handing the
    // validators a weaker (e.g. no-op) sandbox.
    let sandbox = factory.sandbox_for(working_dir, grant);

    // PR B — the agent folds each response's cumulative spend into `tracker` (via
    // `run_task_with_tracker`), so an escalation that ends on the TIMEOUT /
    // run-error path — where the dropped `TaskResult` discards its token count —
    // still settles the REAL tokens used, never 0. `escalate` is a tool call, so
    // an escalation implies at least one response landed and the tracker is
    // populated; a 0-commit would let the fresh post-grant attempt spend the same
    // budget twice. Both `escalation_slot` AND `tracker` are caller-supplied (the
    // pool shares them with the LaunchGuard, so a cancel-drop settles honestly).

    // #1857 PR 5a fix (H1, codex round 2) — ATTEMPT-TIME fail-closed. The serve
    // boot probe checks ONE sandbox instance, but the factory reconstructs the
    // sandbox PER attempt and `SandboxMode::Auto` can fall through to `NoSandbox`
    // if the backend (e.g. bwrap) became unavailable AFTER boot. The closed tool
    // set is a denylist, not a boundary — the shell's network/host reach is
    // bounded ONLY by the sandbox — so a no-op sandbox here means running a fleet
    // worker unsandboxed. REFUSE: settle the attempt `Terminated` (via the normal
    // `complete_child` path below, so the child ends terminal, not silently
    // unsandboxed) WITHOUT ever building or running the agent.
    // codex fix #2b — per-attempt full-FS-write verification for a worktree
    // worker: BOOT guarantees ≠ RUNTIME guarantees. `SandboxMode::Auto`
    // re-resolves the backend per factory call, so the pool's boot-time
    // `repo_git_write_supported` probe can be stale (the backend degraded, e.g.
    // bwrap became unavailable and Auto fell to a backend that can't grant the
    // `.git` write). A worktree worker under such a sandbox would fail its git
    // ops and lose the deliverable. Verify the RESOLVED sandbox actually supports
    // it (mirrors the PR 5a per-attempt network check) and fail closed otherwise.
    let worktree_backend_ok = worktree.is_none() || sandbox.supports_repo_git_write();

    let computed = if sandbox.is_noop() {
        tracing::error!(
            %fleet_id, %task_id, %attempt_id,
            "fleet worker: no isolating sandbox available at attempt time; terminating the \
             attempt instead of running the agent unsandboxed",
        );
        Computed::terminated("no isolating sandbox available at attempt time".to_string())
    } else if !worktree_backend_ok {
        tracing::error!(
            %fleet_id, %task_id, %attempt_id,
            "fleet worker: worktree attempt but the resolved sandbox no longer supports full-FS \
             write (backend degraded since boot); terminating rather than losing the deliverable",
        );
        Computed::terminated(
            "worktree attempt: resolved sandbox no longer supports full-FS write".to_string(),
        )
    } else if let Some(reason) = populate_worktree(worktree, &sandbox, working_dir, deadline).await
    {
        tracing::error!(
            %fleet_id, %task_id, %attempt_id, %reason,
            "fleet worker: worktree populate failed; terminating the attempt",
        );
        Computed::terminated(reason)
    } else {
        // 3. Fresh, granted-registry agent (cannot park, cannot fan out; holds
        //    EXACTLY the operator-granted tools) under the shared sandbox. Its
        //    per-tool timeouts AND per-command shell ceiling are clamped to
        //    `deadline` (P1-2). An incoherent grant (rejected at parse) fails
        //    closed here too: terminate the attempt without running.
        match factory.build_agent(
            working_dir,
            sandbox.clone(),
            deadline,
            &task_view.grant,
            escalation_slot.clone(),
        ) {
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
                //    wrapper is mandatory. `run_task_with_tracker` folds each
                //    response's cumulative tokens into `tracker` as it goes, so a
                //    timeout that DROPS the future still leaves the real spend
                //    readable (budget honesty on the escalation settle below).
                let start = Instant::now();
                let run =
                    tokio::time::timeout(deadline, agent.run_task_with_tracker(&task, &tracker))
                        .await;
                let elapsed = start.elapsed();

                // 4a. DETERMINISM (codex round-2): read the escalation slot
                //     IMMEDIATELY after the turn, BEFORE `run_acceptance`. If the
                //     agent escalated, the attempt YIELDS — SKIP acceptance
                //     entirely (its `CommandExit` validators + side effects must
                //     NOT run, and a cancel during that window must not let the
                //     LaunchGuard record `Terminated`) and settle NON-terminally.
                //     The escalation WINS over any verdict the turn produced.
                let escalation = escalation_slot
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                if let Some(request) = escalation {
                    // Budget honesty: the REAL tokens the yielded attempt used —
                    // the final `TaskResult` count when the turn returned cleanly,
                    // else the tracker, which the agent updated after every
                    // response so it holds the cumulative spend even when a
                    // timeout dropped the `TaskResult`. Take the max so neither
                    // under-reports, with the attempt's reserved tokens as a
                    // never-zero floor (see `escalation_tokens_with_floor`). The
                    // pool's LaunchGuard uses the SAME helper on a cancel-drop.
                    let result_tokens = match &run {
                        Ok(Ok(result)) => total_tokens(&result.token_usage),
                        _ => 0,
                    };
                    let tracked = result_tokens.max(tracked_tokens(&tracker));
                    let tokens = escalation_tokens_with_floor(tracked, reserved_tokens);
                    return settle_escalation(
                        &store,
                        fleet_id,
                        task_id,
                        attempt_id,
                        request,
                        tokens,
                        owner_epoch,
                        actual_now_ms(),
                    )
                    .await;
                }

                // 5-6. Not escalated — map the result to a verdict + snapshot
                //      inputs (acceptance runs only on the clean success path).
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
                            sandbox.clone(),
                            deadline.as_secs().max(1),
                            remaining,
                        )
                        .await;
                        // For a worktree attempt that PASSED acceptance, land the
                        // deliverable on the branch (auto-commit any uncommitted
                        // checkout changes INSIDE the worker's sandbox — §4b) and
                        // REQUIRE the branch to have advanced; an accepted run
                        // that left an EMPTY branch produced no deliverable and
                        // must not be recorded a success. The scratch fallback
                        // (no worktree) passes through unchanged.
                        let (verdict, error) = settle_worktree_deliverable(
                            verdict,
                            error,
                            worktree,
                            &sandbox,
                            working_dir,
                            task_id,
                            deadline,
                        )
                        .await;
                        Computed::from_verdict(verdict, error, &result)
                    }
                }
            }
        }
    };

    // An escalation (if any) was already settled + returned INSIDE the agent arm
    // above, BEFORE acceptance ran — so reaching here means the turn did NOT
    // escalate and the normal verdict applies.
    let now = actual_now_ms();

    // 7. Record the real outcome + snapshot to the store DIRECTLY (not
    //    `Fleet::record_outcome`, which cannot carry the snapshot).
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

/// The live cumulative spend the agent folded into `tracker` after every LLM
/// response (input + output). Read outside any await — the fields are atomics.
/// The pool's [`crate::pool`] LaunchGuard reads the SAME tracker on a cancel so
/// its escalation settle commits the real spend, not 0.
pub(crate) fn tracked_tokens(tracker: &TokenTracker) -> u64 {
    u64::from(tracker.input_tokens.load(Ordering::Relaxed))
        + u64::from(tracker.output_tokens.load(Ordering::Relaxed))
}

/// The tokens an escalation settle should commit — it must be IMPOSSIBLE to
/// settle 0 while real tokens were used. `tracked` is the yielded attempt's REAL
/// live spend, read directly from the shared [`TokenTracker`] the agent updates
/// after every response (an atomic, so always reliable) — on the normal path
/// max'd with the final `TaskResult`. `reserved` is the attempt's RESERVED
/// tokens (the budget-admission estimate), passed in RELIABLY by the pool (it is
/// `PoolConfig::projected_tokens`, the exact amount `launch_child` reserved) —
/// NOT re-fetched via a fallible store read that could zero the floor.
///
/// - `tracked > 0` (real work happened): settle `tracked` — the true spend,
///   never 0, and never inflated to the reservation.
/// - `tracked == 0` (genuinely no tracked spend, e.g. a cancel before any
///   response): fall back to `reserved` as the never-zero floor, so a fresh
///   post-grant attempt can't double-spend the budget.
///
/// A 0-settle is therefore possible ONLY when the attempt genuinely reserved 0
/// AND used 0 — nothing was spent, so there is nothing to re-spend. Shared by
/// `run_attempt` (the normal escalation path) and the pool's LaunchGuard (the
/// cancel path) so BOTH settle honestly from reliable inputs.
pub(crate) fn escalation_tokens_with_floor(tracked: u64, reserved: u64) -> u64 {
    if tracked > 0 { tracked } else { reserved }
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
        // Acceptance validators never call tools, so the escalate valve here is
        // never invoked — a throwaway slot suffices (the agent path threads the
        // real one). Acceptance only runs on a NON-escalated turn anyway.
        Arc::new(Mutex::new(None)),
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

/// Cap for an in-sandbox worktree git op (populate/commit). Bounded so a hung
/// clean/smudge filter or `post-commit` hook can't retain pool permits; the
/// effective deadline is `min(this, attempt deadline)` (git ops are fast, so
/// this is a generous ceiling that only binds a pathological hang).
const WORKTREE_GIT_OP_DEADLINE: Duration = Duration::from_secs(120);

/// Build the sandboxed git-op command: wrap it in the worker's sandbox, put it in
/// its own process group (so the timeout can group-kill a detached filter/hook
/// child), and STRIP controller secrets from its environment.
fn sandboxed_git_command(
    sandbox: &Arc<dyn Sandbox>,
    cwd: &Path,
    shell_cmd: &str,
) -> tokio::process::Command {
    let mut command = sandbox.wrap_command(shell_cmd, cwd);
    #[cfg(unix)]
    {
        // Own process group so the timeout can SIGTERM→SIGKILL the whole tree
        // (a planted filter/hook may detach a child).
        command.process_group(0);
    }
    // HIGH (controller-hijack, fix 1): strip provider/API keys + injection vars
    // exactly as the shell tool does, so a worker-planted `.git` `filter.*`/hook
    // that dumps `env` during the populate/commit never sees controller secrets
    // (a Full-network worker would otherwise exfiltrate them).
    octos_agent::sanitize_default_subprocess_env(&mut command);
    command
}

/// Run a git shell command INSIDE the worker's sandbox, BOUNDED by
/// `min(deadline, WORKTREE_GIT_OP_DEADLINE)` with a cancellation-safe
/// process-group kill on timeout (codex fix #4; mirrors the command-validator
/// timeout). The child is spawned in its own process group so a detached
/// filter/hook child is reaped too.
///
/// `require_success` decides how a CLEAN (non-timeout) exit is judged:
/// - `true` (the POPULATE, `git reset --hard`): a NON-ZERO exit is a FAILURE
///   (`Err`) — an unpopulated tree means the agent would run in a partial/empty
///   checkout, so the attempt must fail rather than proceed (fix 4).
/// - `false` (the deliverable COMMIT): the exit code is NOT gated on — a clean
///   tree is a deliberate no-op (exit 0) and a worker's own broken `filter.*`
///   (contained) must not read as infra failure; the authoritative check is
///   whether the branch advanced, read host-side.
///
/// `Err` also on a spawn failure or a DEADLINE (the child + its group are
/// SIGTERM→SIGKILL'd).
async fn run_sandboxed_git(
    sandbox: &Arc<dyn Sandbox>,
    cwd: &Path,
    shell_cmd: &str,
    deadline: Duration,
    require_success: bool,
) -> Result<(), String> {
    let bounded = deadline
        .min(WORKTREE_GIT_OP_DEADLINE)
        .max(Duration::from_secs(1));
    let mut command = sandboxed_git_command(sandbox, cwd, shell_cmd);
    let mut child = command
        .spawn()
        .map_err(|e| format!("failed to spawn sandboxed git op: {e}"))?;
    let pid = child.id();
    match tokio::time::timeout(bounded, child.wait()).await {
        Ok(Ok(status)) => {
            if require_success && !status.success() {
                Err(format!("sandboxed git op exited with {status}"))
            } else {
                Ok(())
            }
        }
        Ok(Err(e)) => Err(format!("sandboxed git op wait failed: {e}")),
        Err(_elapsed) => {
            if let Some(pid) = pid {
                octos_agent::kill_child_process(pid).await;
            }
            Err(format!(
                "sandboxed git op timed out after {}s (killed)",
                bounded.as_secs()
            ))
        }
    }
}

/// codex fix #5 — POPULATE a `--no-checkout` worktree's working tree INSIDE the
/// worker's sandbox (via `git reset --hard`), so a worker-planted local smudge
/// filter runs at the WORKER's grant (contained), never host-side as the daemon
/// during the controller's `git worktree add`. Returns `None` on success (or the
/// scratch fallback, `worktree = None`); `Some(reason)` on failure/timeout, so
/// the caller terminates the attempt rather than run the agent in an empty tree.
async fn populate_worktree(
    worktree: Option<&WorktreeContext>,
    sandbox: &Arc<dyn Sandbox>,
    checkout: &Path,
    deadline: Duration,
) -> Option<String> {
    worktree?;
    let cmd = octos_core::worktree_populate_command();
    // require_success = true (fix 4): a non-zero `git reset --hard` left the tree
    // unpopulated — FAIL the attempt rather than run the agent in an empty tree.
    match run_sandboxed_git(sandbox, checkout, &cmd, deadline, true).await {
        Ok(()) => None,
        Err(err) => Some(format!("worktree populate failed: {err}")),
    }
}

/// Land + verify a WORKTREE attempt's deliverable. For an `Accepted` verdict
/// with a worktree context: run the auto-commit fallback (`git add -A &&
/// git commit`) INSIDE the worker's sandbox — so a worker-planted local-config
/// `filter.*`/commit hook triggered by it runs at the WORKER's network grant
/// (contained), NOT host-side with the controller's network (§4b, mixed-grant
/// escape dissolution) — then REQUIRE the branch to have advanced past its base:
///
/// - branch advanced → keep `Accepted` (the deliverable is durable on the
///   branch, which survives the later checkout removal);
/// - branch UNCHANGED (empty) → downgrade to `Rejected` — an accepted run that
///   produced no commit is not a real deliverable and must not be recorded a
///   success on an empty `fleet/*` branch;
/// - branch-read ERROR (infra) → `Terminated`.
///
/// The commit command's own exit code is NOT gated on: a clean tree is a
/// deliberate no-op (exit 0), and a worker's own broken filter (contained) must
/// not be an infra error — the authoritative check is whether the branch
/// advanced, read host-side (hooks-disabled). A non-`Accepted` verdict, or the
/// scratch fallback (`worktree = None`), passes through unchanged.
async fn settle_worktree_deliverable(
    verdict: AcceptanceVerdict,
    error: Option<String>,
    worktree: Option<&WorktreeContext>,
    sandbox: &Arc<dyn Sandbox>,
    checkout: &Path,
    task_id: &str,
    deadline: Duration,
) -> (AcceptanceVerdict, Option<String>) {
    let Some(ctx) = worktree else {
        return (verdict, error);
    };
    if !matches!(verdict, AcceptanceVerdict::Accepted { .. }) {
        return (verdict, error);
    }

    // (b) Auto-commit any uncommitted deliverable INSIDE the worker's sandbox,
    // BOUNDED by a deadline with a cancellation-safe process-group kill (codex
    // fix #4): a worker-planted clean/process filter or `post-commit` hook fired
    // here must not HANG forever (retaining pool permits + leaving the child
    // Running). A clean tree is a no-op. A spawn failure or a DEADLINE is infra
    // → Terminated; a non-zero EXIT is not gated on (the branch-advance check
    // below is authoritative).
    let message = format!("fleet {task_id} deliverable");
    let commit_cmd = octos_core::deliverable_commit_command(&message);
    // require_success = false: the commit's exit code is NOT authoritative (a
    // clean tree no-ops at exit 0; a worker's own broken filter is contained, not
    // infra). The branch-advance check below is the authority.
    if let Err(err) = run_sandboxed_git(sandbox, checkout, &commit_cmd, deadline, false).await {
        let reason = format!("worktree deliverable auto-commit failed: {err}");
        tracing::error!(%reason, "fleet worker: deliverable auto-commit error");
        return (
            AcceptanceVerdict::Terminated {
                reason: reason.clone(),
            },
            Some(reason),
        );
    }

    // Authoritative: did the branch advance past its base? (Read host-side,
    // hooks-disabled — a read-only `rev-parse`, no code-exec surface.)
    match octos_core::branch_advanced_past(&ctx.repo_root, &ctx.branch, &ctx.base_commit) {
        Ok(true) => (verdict, error),
        Ok(false) => {
            let reason = format!(
                "accepted run left an EMPTY fleet branch — no commit / no changes to commit \
                 for task {task_id}; refusing to record a phantom deliverable"
            );
            tracing::warn!(%reason, "fleet worker: empty worktree deliverable");
            (
                AcceptanceVerdict::Rejected {
                    reason: reason.clone(),
                },
                Some(reason),
            )
        }
        Err(err) => {
            let reason = format!("worktree deliverable branch check failed: {err}");
            tracing::error!(%reason, "fleet worker: deliverable branch check error");
            (
                AcceptanceVerdict::Terminated {
                    reason: reason.clone(),
                },
                Some(reason),
            )
        }
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

    /// A fresh, empty escalation slot for a `run_attempt` call. The agent writes
    /// into it via the `escalate` valve; `run_attempt` reads it after the turn.
    fn slot() -> EscalationSlot {
        Arc::new(Mutex::new(None))
    }

    /// A fresh, shared token tracker for a `run_attempt` call. The pool creates
    /// one per attempt and shares it into the LaunchGuard; these direct-call
    /// tests only need the agent to have somewhere to fold its token counts.
    fn tracker() -> Arc<TokenTracker> {
        Arc::new(TokenTracker::new())
    }

    #[test]
    fn sandboxed_git_env_is_sanitized_of_secrets() {
        // HIGH (controller-hijack): a worker's sandboxed git op (populate/commit)
        // can trigger a planted `.git` `filter.*`/hook that dumps `env`. That op
        // must NOT inherit the CONTROLLER's provider/API keys — else a
        // Full-network worker exfiltrates them. Re-exec THIS test binary with a
        // provider key in its env (mirroring the daemon that holds one) and
        // assert the child, running through `sandboxed_git_command`, can't see it.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("worker::tests::child_sandboxed_git_env_has_no_secret")
            .arg("--exact")
            .arg("--ignored")
            .env("OPENAI_API_KEY", "sk-fleet-controller-secret")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child regression failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[tokio::test]
    #[ignore]
    async fn child_sandboxed_git_env_has_no_secret() {
        // Runs in a re-exec'd process that HAS `OPENAI_API_KEY` in its env. The
        // sandboxed git command dumps `env` (a stand-in for a worker-planted
        // `filter.*.clean` doing the same); the controller secret must be gone.
        let sandbox: Arc<dyn Sandbox> = Arc::new(NoSandbox);
        let cwd = std::env::temp_dir();
        let mut cmd = sandboxed_git_command(&sandbox, &cwd, "env");
        let out = cmd.output().await.expect("run env dump");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            !text.contains("sk-fleet-controller-secret"),
            "sandboxed git op leaked the controller API key to a planted filter:\n{text}"
        );
        // Sanity: the dump ran and produced SOME environment, so the negative
        // assertion above is not vacuously true because `env` failed to run.
        assert!(!text.is_empty(), "env dump produced no output");
    }

    #[tokio::test]
    async fn run_sandboxed_git_gates_exit_status_when_required() {
        // fix 4: a non-zero exit FAILS only when `require_success` (the populate);
        // the commit path (false) treats a clean non-zero exit as Ok (its exit
        // code is not authoritative — the branch-advance check is).
        let sandbox: Arc<dyn Sandbox> = Arc::new(NoSandbox);
        let cwd = std::env::temp_dir();
        let d = Duration::from_secs(30);

        let gated = run_sandboxed_git(&sandbox, &cwd, "exit 7", d, true).await;
        assert!(
            gated.is_err(),
            "a non-zero populate must be Err, got {gated:?}"
        );

        let ungated = run_sandboxed_git(&sandbox, &cwd, "exit 7", d, false).await;
        assert!(
            ungated.is_ok(),
            "the commit path must NOT gate on exit code, got {ungated:?}"
        );

        // A clean success is Ok under either flag.
        assert!(
            run_sandboxed_git(&sandbox, &cwd, "true", d, true)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn failed_populate_fails_the_attempt() {
        // fix 4 (regression): populating a `--no-checkout` worktree runs
        // `git reset --hard`. In a NON-repo dir that exits non-zero; the populate
        // must report failure (Some) so `run_attempt` TERMINATES rather than run
        // the agent in an empty/partial tree.
        let sandbox: Arc<dyn Sandbox> = Arc::new(NoSandbox);
        let dir = TempDir::new().unwrap(); // deliberately NOT a git repo
        let ctx = WorktreeContext {
            repo_root: dir.path().to_path_buf(),
            branch: "fleet/f/t".to_string(),
            base_commit: "0".repeat(40),
            git_dir: dir.path().join(".git"),
        };
        let reason =
            populate_worktree(Some(&ctx), &sandbox, dir.path(), Duration::from_secs(30)).await;
        assert!(
            reason.is_some(),
            "a failed populate must fail the attempt (Some(reason)), got None"
        );
        assert!(
            reason.as_deref().unwrap().contains("populate failed"),
            "unexpected reason: {reason:?}"
        );
    }

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
    async fn grant_threads_network_and_repo_git_dir_into_sandbox() {
        // `run_attempt` derives the sandbox scope PER ATTEMPT: `allow_network`
        // from the task's network lane (PR A), and `repo_git_dir` from whether
        // this is an actual WORKTREE attempt (`worktree.is_some()`, codex fix
        // #2a) — NOT from `fs.is_host()` alone, so a scratch worker is never
        // handed the extra `.git` write. A recording factory captures the
        // `SandboxGrant`.
        use std::sync::Mutex;
        let seen: Arc<Mutex<Vec<SandboxGrant>>> = Arc::new(Mutex::new(Vec::new()));

        async fn run_with(
            seen: Arc<std::sync::Mutex<Vec<SandboxGrant>>>,
            grant: octos_fleet::WorkerGrant,
            worktree: Option<WorktreeContext>,
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
                Arc::new(move |_cwd, grant: SandboxGrant| {
                    rec.lock().unwrap().push(grant);
                    Arc::new(MarkerSandbox) as Arc<dyn Sandbox>
                }),
            );
            let task_view = view_of(&fleet, "a").await;
            let work = TempDir::new().unwrap();
            // The factory records the grant BEFORE any populate/agent step, so a
            // dummy worktree ctx (populate will no-op/fail on this non-repo tmp
            // dir) still captures the intended `repo_git_dir`.
            let _ = run_attempt(
                store.clone(),
                "f1",
                "a",
                &attempt,
                &task_view,
                &factory,
                work.path(),
                worktree.as_ref(),
                Duration::from_secs(30),
                EPOCH,
                slot(),
                tracker(),
                PROJECTED,
                || NOW,
            )
            .await;
        }

        let dummy_worktree = || WorktreeContext {
            repo_root: std::path::PathBuf::from("/nonexistent-repo"),
            branch: "fleet/f1/a".to_string(),
            base_commit: "0000000".to_string(),
            git_dir: std::path::PathBuf::from("/nonexistent-repo/.git"),
        };

        // Minimal grant, scratch (no worktree) → no raw egress, cwd-only.
        run_with(seen.clone(), octos_fleet::WorkerGrant::minimal(), None).await;
        // Full network grant, scratch → raw egress, but STILL cwd-only (no worktree).
        run_with(
            seen.clone(),
            octos_fleet::WorkerGrant {
                network: octos_fleet::NetworkGrant::Full,
                ..octos_fleet::WorkerGrant::minimal()
            },
            None,
        )
        .await;
        // A worktree attempt → repo_git_dir set to the checkout's `<repo>/.git`.
        run_with(
            seen.clone(),
            octos_fleet::WorkerGrant {
                network: octos_fleet::NetworkGrant::Full,
                fs: octos_fleet::FsGrant::Host,
                ..octos_fleet::WorkerGrant::minimal()
            },
            Some(dummy_worktree()),
        )
        .await;

        let grants = seen.lock().unwrap().clone();
        assert!(
            grants.contains(&SandboxGrant {
                allow_network: false,
                repo_git_dir: None,
                write_allow_globs: None,
            }),
            "a minimal scratch worker gets the base sandbox (no network, cwd-only): {grants:?}",
        );
        assert!(
            grants.contains(&SandboxGrant {
                allow_network: true,
                repo_git_dir: None,
                write_allow_globs: None,
            }),
            "a Full-network SCRATCH worker gets network but NOT the .git write: {grants:?}",
        );
        assert!(
            grants.contains(&SandboxGrant {
                allow_network: true,
                repo_git_dir: Some(std::path::PathBuf::from("/nonexistent-repo/.git")),
                write_allow_globs: None,
            }),
            "a WORKTREE attempt gets repo_git_dir = <repo>/.git: {grants:?}",
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
            None,
            Duration::from_secs(30),
            EPOCH,
            slot(),
            tracker(),
            PROJECTED,
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
            None,
            Duration::from_secs(30),
            EPOCH,
            slot(),
            tracker(),
            PROJECTED,
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
            None,
            Duration::from_secs(30),
            EPOCH,
            slot(),
            tracker(),
            PROJECTED,
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
            None,
            Duration::from_secs(2),
            EPOCH,
            slot(),
            tracker(),
            PROJECTED,
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
            None,
            Duration::from_secs(2),
            EPOCH,
            slot(),
            tracker(),
            PROJECTED,
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
            None,
            Duration::from_secs(30),
            EPOCH,
            slot(),
            tracker(),
            PROJECTED,
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
            None,
            Duration::from_secs(30),
            EPOCH,
            slot(),
            tracker(),
            PROJECTED,
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
            None,
            Duration::from_secs(30),
            EPOCH,
            slot(),
            tracker(),
            PROJECTED,
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
            None,
            Duration::from_millis(200),
            EPOCH,
            slot(),
            tracker(),
            PROJECTED,
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
            None,
            Duration::from_secs(30),
            EPOCH,
            slot(),
            tracker(),
            PROJECTED,
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
            None,
            Duration::from_secs(30),
            EPOCH,
            slot(),
            tracker(),
            PROJECTED,
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

    #[tokio::test]
    async fn escalate_records_request_and_yields_attempt() {
        // A worker that calls the always-on `escalate` tool must yield its
        // attempt NON-terminally: run_attempt settles via record_escalation
        // (child Blocked + pending_escalation, real tokens committed), NOT the
        // normal verdict map — even though the turn ended cleanly (EndTurn).
        let (_sd, store) = fresh_store().await;
        let fleet = create_fleet(store.clone(), "f1", vec![task_spec("a", &[], vec![])]).await;
        let attempt = launch(&store, "f1", "a").await;

        let work = TempDir::new().unwrap();
        let (_md, factory) =
            factory_for(Arc::new(EscalateProvider::new("cannot reach example.com"))).await;
        let task_view = view_of(&fleet, "a").await;

        let outcome = run_attempt(
            store.clone(),
            "f1",
            "a",
            &attempt,
            &task_view,
            &factory,
            work.path(),
            None,
            Duration::from_secs(30),
            EPOCH,
            slot(),
            tracker(),
            PROJECTED,
            || NOW,
        )
        .await;

        // The disposition is Escalated (NOT Completed) — the verdict is ignored.
        match &outcome {
            AttemptOutcome::Escalated { request } => {
                assert_eq!(request.reason, "cannot reach example.com");
                assert_eq!(
                    request.requested_grant.network,
                    octos_fleet::NetworkGrant::Hosts(vec!["example.com".into()]),
                );
            }
            other => panic!("expected Escalated, got {other:?}"),
        }

        // Child is Blocked (non-terminal) with the pending request; the attempt
        // Interrupted; the REAL tokens are committed (budget honesty).
        let child = store.get_child("f1", "a").await.unwrap().unwrap();
        assert_eq!(child.status, ChildStatus::Blocked);
        assert!(
            child.pending_escalation.is_some(),
            "request recorded on child"
        );
        let att = store.get_attempt("a", &attempt).await.unwrap().unwrap();
        assert_eq!(att.status, AttemptStatus::Interrupted);
        let fleet_rec = store.get_fleet("f1").await.unwrap().unwrap();
        assert!(
            fleet_rec.budget.tokens_committed > 0,
            "record_escalation must settle the REAL tokens used, not 0",
        );
        assert_eq!(fleet_rec.budget.tokens_reserved, 0, "reservation released");
    }

    #[test]
    fn escalation_floor_never_loses_real_usage_uses_reservation_only_when_absent() {
        // codex round-4 (defect 1): the pure floor. Real tracked usage always
        // wins (never 0, never inflated to the reservation); the reservation is
        // the floor ONLY when the tracker is genuinely 0; a 0-settle is possible
        // ONLY when BOTH are 0 (nothing spent → nothing to re-spend).
        assert_eq!(
            escalation_tokens_with_floor(10, 0),
            10,
            "real usage with a 0 reservation must settle the usage, never 0",
        );
        assert_eq!(
            escalation_tokens_with_floor(10, 500),
            10,
            "real usage is NOT inflated to the reservation (only floor when absent)",
        );
        assert_eq!(
            escalation_tokens_with_floor(0, 100),
            100,
            "no tracked usage floors to the reservation (never under-settle it)",
        );
        assert_eq!(
            escalation_tokens_with_floor(0, 0),
            0,
            "reserved 0 AND used 0 → 0 is the only acceptable 0-settle",
        );
    }

    #[tokio::test]
    async fn escalation_settle_never_zero_when_tokens_used() {
        // codex round-4 (defect 1): a real-usage attempt that escalates must NEVER
        // commit 0 — even when it RESERVED 0 tokens. The settle reads the live
        // shared tracker (the agent's real spend) reliably; it can only settle 0
        // when reserved==0 AND nothing was tracked. Here the provider reports real
        // usage, so the committed tokens must be > 0 despite the 0 reservation —
        // otherwise the reservation would be released while committing nothing and
        // a fresh post-grant attempt could double-spend.
        let (_sd, store) = fresh_store().await;
        let fleet = create_fleet(store.clone(), "f1", vec![task_spec("a", &[], vec![])]).await;

        // Launch with a ZERO reservation (projected_tokens = 0) — the corner the
        // reservation floor cannot cover, so real usage MUST carry the settle.
        let attempt = match store
            .launch_child("f1", "a", 0, NOW, EPOCH, TTL)
            .await
            .unwrap()
        {
            octos_fleet::LaunchOutcome::Launched { attempt_id } => attempt_id,
            other => panic!("expected Launched, got {other:?}"),
        };

        let work = TempDir::new().unwrap();
        // Escalates on turn 1 with real usage (7 in + 3 out = 10 tracked tokens).
        let (_md, factory) = factory_for(Arc::new(EscalateProvider::new(
            "blocked, but I did real work",
        )))
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
            None,
            Duration::from_secs(30),
            EPOCH,
            slot(),
            tracker(),
            0, // reserved_tokens: this attempt reserved ZERO
            || NOW,
        )
        .await;
        assert!(
            matches!(outcome, AttemptOutcome::Escalated { .. }),
            "got {outcome:?}",
        );

        // Despite the 0 reservation, the REAL tracked usage was committed (> 0).
        let fleet_rec = store.get_fleet("f1").await.unwrap().unwrap();
        assert!(
            fleet_rec.budget.tokens_committed > 0,
            "a real-usage escalation must commit > 0 even with a 0 reservation (committed={})",
            fleet_rec.budget.tokens_committed,
        );
        assert_eq!(
            store.get_child("f1", "a").await.unwrap().unwrap().status,
            ChildStatus::Blocked,
        );
    }

    #[tokio::test]
    async fn escalation_wins_regardless_of_turn_end() {
        // DETERMINISM: even when the turn ends by hitting the DEADLINE (not a
        // clean EndTurn), a recorded escalation still wins — run_attempt settles
        // escalated, never Terminated. The provider escalates on turn 1, then
        // sleeps 30s so the 2s deadline fires while the slot is already set.
        let (_sd, store) = fresh_store().await;
        let fleet = create_fleet(store.clone(), "f1", vec![task_spec("a", &[], vec![])]).await;
        let attempt = launch(&store, "f1", "a").await;

        let work = TempDir::new().unwrap();
        let (_md, factory) = factory_for(Arc::new(EscalateProvider::then_sleeping(
            "blocked past the deadline",
            Duration::from_secs(30),
        )))
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
            None,
            Duration::from_secs(2),
            EPOCH,
            slot(),
            tracker(),
            PROJECTED,
            || NOW,
        )
        .await;

        assert!(
            matches!(outcome, AttemptOutcome::Escalated { .. }),
            "a deadline-ended turn with a set escalation slot must still escalate, got {outcome:?}",
        );
        let child = store.get_child("f1", "a").await.unwrap().unwrap();
        assert_eq!(
            child.status,
            ChildStatus::Blocked,
            "the escalation wins over the deadline terminate",
        );
        // BUDGET HONESTY (codex round-2): even though the turn ended by TIMEOUT
        // (the TaskResult was dropped), the escalation settled the REAL tokens the
        // agent spent BEFORE the timeout — captured live via the CostUpdate
        // reporter — NOT 0. A 0-commit would let the fresh post-grant attempt
        // spend the whole budget twice.
        let fleet_rec = store.get_fleet("f1").await.unwrap().unwrap();
        assert!(
            fleet_rec.budget.tokens_committed > 0,
            "a timeout-escalation must settle the REAL tokens used, never 0",
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn escalation_skips_acceptance_no_validator_side_effect() {
        // DETERMINISM (codex round-2): an escalated attempt must SKIP the
        // acceptance gate entirely — its `CommandExit` validators (and their side
        // effects) must NOT run. Proof: a CommandExit whose command WRITES a
        // marker; on an escalated turn the marker is NEVER written.
        use std::os::unix::fs::PermissionsExt;

        let (_sd, store) = fresh_store().await;
        let scratch = TempDir::new().unwrap();
        let script = scratch.path().join("mark.sh");
        let marker = scratch.path().join("acceptance-ran.marker");
        std::fs::write(&script, format!("#!/bin/sh\n: > {}\n", marker.display())).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let fleet = create_fleet(
            store.clone(),
            "f1",
            vec![task_spec("a", &[], command_exit(&script.to_string_lossy()))],
        )
        .await;
        let attempt = launch(&store, "f1", "a").await;

        let work = TempDir::new().unwrap();
        // The provider escalates on turn 1 then ends — so the run RETURNS success,
        // which would normally trigger acceptance. Because run_attempt reads the
        // slot BEFORE acceptance, the validator (marker script) must NOT run.
        let (_md, factory) = factory_for(Arc::new(EscalateProvider::new("blocked"))).await;
        let task_view = view_of(&fleet, "a").await;

        let outcome = run_attempt(
            store.clone(),
            "f1",
            "a",
            &attempt,
            &task_view,
            &factory,
            work.path(),
            None,
            Duration::from_secs(30),
            EPOCH,
            slot(),
            tracker(),
            PROJECTED,
            || NOW,
        )
        .await;
        assert!(
            matches!(outcome, AttemptOutcome::Escalated { .. }),
            "got {outcome:?}",
        );
        // Give any (erroneously-spawned) validator time to write before asserting.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !marker.exists(),
            "acceptance validator ran on an escalated attempt — its side effect leaked",
        );
        assert_eq!(
            store.get_child("f1", "a").await.unwrap().unwrap().status,
            ChildStatus::Blocked,
        );
    }

    #[tokio::test]
    async fn fresh_attempt_after_grant_widen_rebuilds_from_new_grant() {
        // The whole loop: a minimal-grant worker escalates → Blocked; the keeper
        // widens the grant (SetGrant → Ready); a FRESH attempt rebuilds its
        // registry from the NEW grant, so it is now offered the widened tools
        // (web_fetch) it lacked before.
        let (_sd, store) = fresh_store().await;
        let fleet = create_fleet(store.clone(), "f1", vec![task_spec("a", &[], vec![])]).await;

        // Attempt 1: minimal grant → escalate → Blocked.
        let a1 = launch(&store, "f1", "a").await;
        let work = TempDir::new().unwrap();
        let (_md1, esc_factory) = factory_for(Arc::new(EscalateProvider::new(
            "need web_fetch for example.com",
        )))
        .await;
        let view1 = view_of(&fleet, "a").await;
        assert!(
            !view1.grant.tools.contains(&"web_fetch".to_string()),
            "attempt 1 is minimal — no web_fetch",
        );
        let out1 = run_attempt(
            store.clone(),
            "f1",
            "a",
            &a1,
            &view1,
            &esc_factory,
            work.path(),
            None,
            Duration::from_secs(30),
            EPOCH,
            slot(),
            tracker(),
            PROJECTED,
            || NOW,
        )
        .await;
        assert!(
            matches!(out1, AttemptOutcome::Escalated { .. }),
            "got {out1:?}"
        );

        // Keeper approves a WIDER grant (adds web_fetch under a Hosts allowlist)
        // via the targeted SetGrant edit → Blocked → Ready.
        let widened = octos_fleet::WorkerGrant {
            network: octos_fleet::NetworkGrant::Hosts(vec!["example.com".into()]),
            tools: vec![
                "read_file".into(),
                "write_file".into(),
                "shell".into(),
                "web_fetch".into(),
            ],
            ..octos_fleet::WorkerGrant::minimal()
        };
        let rev = fleet.view().await.unwrap().revision;
        let edit = fleet
            .apply_edit(
                octos_fleet::PlanEdit::SetGrant {
                    task_id: "a".into(),
                    grant: widened.clone(),
                },
                rev,
                NOW,
            )
            .await
            .unwrap();
        assert!(matches!(
            edit,
            octos_fleet::PlanMutateOutcome::Mutated { .. }
        ));

        // Attempt 2: FRESH launch (the child is Ready again) — its registry must
        // be rebuilt from the WIDER grant, so the agent is offered web_fetch.
        let a2 = launch(&store, "f1", "a").await;
        assert_ne!(a1, a2, "a fresh attempt id");
        let view2 = view_of(&fleet, "a").await;
        assert_eq!(view2.grant, widened, "attempt 2 carries the widened grant");
        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (_md2, write_factory) = factory_for(Arc::new(RecordingWriteProvider::new(
            "out.txt",
            seen.clone(),
        )))
        .await;
        let work2 = TempDir::new().unwrap();
        let out2 = run_attempt(
            store.clone(),
            "f1",
            "a",
            &a2,
            &view2,
            &write_factory,
            work2.path(),
            None,
            Duration::from_secs(30),
            EPOCH,
            slot(),
            tracker(),
            PROJECTED,
            || NOW,
        )
        .await;
        assert!(
            matches!(out2, AttemptOutcome::Completed { .. }),
            "the re-run must complete, got {out2:?}",
        );
        let offered = seen.lock().unwrap().clone();
        assert!(
            offered.contains(&"web_fetch".to_string()),
            "the fresh attempt must be rebuilt from the WIDENED grant (web_fetch offered): {offered:?}",
        );
        assert!(
            offered.contains(&"escalate".to_string()),
            "the escalate valve is still always present: {offered:?}",
        );
    }
}
