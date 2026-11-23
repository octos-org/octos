//! Budget tracking and enforcement for the agent loop.

use std::time::{Duration, Instant};

use octos_core::TokenUsage;
use tracing::{info, warn};

use super::Agent;
use super::activity::{DEFAULT_IDLE_TIMEOUT_SECS, LoopActivityState};
use crate::progress::ProgressEvent;

/// Reason why the agent loop stopped due to budget constraints.
pub(super) enum BudgetStop {
    Shutdown,
    /// The agent reached its per-turn iteration limit (`max_iterations`).
    ///
    /// We carry the limit so the user-facing message can name the exact
    /// number of iterations that elapsed and the user (or downstream
    /// tool) can recognise this as the iteration-cap path rather than
    /// the bare "Reached max iterations." stub that pre-2026-05 builds
    /// emitted. See the silent-failure bug where a missing `run_pipeline`
    /// pipeline rejection cascaded into ~20 rounds of manual `web_fetch`
    /// until the loop hit `max_iterations` and persisted only "Reached
    /// max iterations." as the assistant reply.
    MaxIterations {
        limit: u32,
    },
    MaxTokens {
        used: u32,
        limit: u32,
    },
    ActivityTimeout {
        limit: Duration,
    },
    IdleProgressTimeout {
        limit: Duration,
    },
}

impl BudgetStop {
    pub(super) fn message(&self) -> String {
        match self {
            Self::Shutdown => String::new(),
            Self::MaxIterations { limit } => {
                // Report the configured limit without treating the stop as
                // completion or encouraging automatic budget-reset retries.
                format!(
                    "The agent did not complete within {limit} iterations. \
                     Preserve any partial artifacts and report the remaining work. \
                     Continue only under the user's authorized budget; do not \
                     automatically re-spawn to reset this limit. For an explicitly \
                     unlimited run, max_iterations=0 disables the iteration cap."
                )
            }
            Self::MaxTokens { used, limit } => {
                format!("Token budget exceeded ({used} of {limit}).")
            }
            Self::ActivityTimeout { limit } => {
                format!("Activity timeout ({:.0}s limit).", limit.as_secs_f64())
            }
            Self::IdleProgressTimeout { limit } => {
                format!(
                    "Idle progress timeout ({:.0}s without progress).",
                    limit.as_secs_f64()
                )
            }
        }
    }
}

/// Tokens counted against the turn token budget: everything the provider
/// processed. Per the [`octos_llm::TokenUsage`] contract, cache counts are
/// DISJOINT from `input_tokens` on every provider (total prompt = input +
/// cache_read + cache_write; inclusive wire formats are normalized at their
/// parse boundary), so this sum is exact. Counting only input+output would
/// let a cache-served Anthropic loop run ~10x past the cap that bounded it
/// before prompt caching landed.
fn budget_tokens_used(total_usage: &TokenUsage) -> u32 {
    total_usage
        .input_tokens
        .saturating_add(total_usage.output_tokens)
        .saturating_add(total_usage.cache_read_tokens)
        .saturating_add(total_usage.cache_write_tokens)
}

impl Agent {
    /// Check whether the agent loop should stop due to budget constraints.
    pub(super) fn check_budget(
        &self,
        iteration: u32,
        start: Instant,
        total_usage: &TokenUsage,
        activity: &LoopActivityState,
    ) -> Option<BudgetStop> {
        use std::sync::atomic::Ordering;

        if self.shutdown.load(Ordering::Acquire) {
            return Some(BudgetStop::Shutdown);
        }
        // `0` disables the iteration cap, including for spawned agents.
        // Explicit host limits, cancellation, idle detection and per-call
        // timeouts retain their own independent semantics.
        if self.config.max_iterations > 0 && iteration >= self.config.max_iterations {
            return Some(BudgetStop::MaxIterations {
                limit: self.config.max_iterations,
            });
        }
        let idle_timeout = Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS);
        if activity.has_timed_out(idle_timeout) {
            return Some(BudgetStop::IdleProgressTimeout {
                limit: idle_timeout,
            });
        }
        if let Some(timeout) = self.config.max_timeout {
            if start.elapsed() > timeout && !activity.recently_active_within(timeout) {
                return Some(BudgetStop::ActivityTimeout { limit: timeout });
            }
        }
        if let Some(max_tokens) = self.config.max_tokens {
            let used = budget_tokens_used(total_usage);
            if used >= max_tokens {
                return Some(BudgetStop::MaxTokens {
                    used,
                    limit: max_tokens,
                });
            }
        }
        None
    }

    /// Log and report a budget stop event (used by `run_task`).
    pub(super) fn report_budget_stop(&self, stop: &BudgetStop, iteration: u32) {
        match stop {
            BudgetStop::Shutdown => {
                info!(iteration, "shutdown signal received");
                self.reporter().report(ProgressEvent::TaskInterrupted {
                    iterations: iteration,
                });
            }
            BudgetStop::MaxIterations { limit } => {
                warn!(iteration, max = *limit, "hit max iterations limit");
                self.reporter()
                    .report(ProgressEvent::MaxIterationsReached { limit: *limit });
            }
            BudgetStop::MaxTokens { used, limit } => {
                warn!(used, max = limit, "hit token budget limit");
                self.reporter().report(ProgressEvent::TokenBudgetExceeded {
                    used: *used,
                    limit: *limit,
                });
            }
            BudgetStop::ActivityTimeout { limit } => {
                warn!(limit_s = limit.as_secs(), "hit activity timeout");
                self.reporter()
                    .report(ProgressEvent::ActivityTimeoutReached {
                        elapsed: *limit,
                        limit: *limit,
                    });
            }
            BudgetStop::IdleProgressTimeout { limit } => {
                warn!(limit_s = limit.as_secs(), "hit idle progress timeout");
                self.reporter().report(ProgressEvent::TaskInterrupted {
                    iterations: iteration,
                });
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// #27e (R2) — budget-exhaustion checkpoint: never lose a dirty worktree.
// ─────────────────────────────────────────────────────────────────────────

/// #27e — run `git <args>` in `dir`, returning success + trimmed stdout.
fn git_in(dir: &std::path::Path, args: &[&str]) -> Option<String> {
    // Kernel-side git in a repo the agent can write: repository-scope hooks,
    // fsmonitor and filter drivers are overridden (see `agent_repo_git`).
    octos_core::agent_repo_git::agent_repo_git(dir)
        .args(args)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// #27e — write `result.md` ATOMICALLY (tmp + rename) so a concurrent peer
/// writer or reader never observes a torn file. The exclusivity/mutex half
/// of the R3 fix lives in a separate slice (#27f); this is the atomic
/// prerequisite the 27e ticket explicitly requires ("先原子后互斥").
fn write_result_md_named(dir: &std::path::Path, name: &str, body: &str) -> std::io::Result<()> {
    let final_path = dir.join(name);
    let tmp_path = dir.join(format!(".{name}.tmp-27e"));
    std::fs::write(&tmp_path, body)?;
    std::fs::rename(&tmp_path, &final_path)
}

/// #27e — checkpoint a budget-exhausted turn against a DIRTY worktree:
///
/// ① if `workdir` is a git repo with uncommitted changes, auto-commit them
///    (`wip: budget exhausted (#27e) — checkpointed mid-task`, never pushed);
/// ② atomically write a STAGED `result.md` naming what was done, what
///    remains, and that the turn ended `budget_exhausted` — so the outer
///    loop can decide to re-dispatch instead of re-discovering the work;
/// ③ the caller stamps the `TaskResult` with the distinct
///    `budget_exhausted` marker so the terminal status is recognisable in
///    the event stream (vs a generic failure).
///
/// Checkpointing does not change the configured budget. NOTHING is pushed;
/// a CLEAN worktree produces
/// NO empty commit (the git call is skipped when `status --porcelain` is
/// empty) and no result.md overwrite.
/// #27h/27h-r1 — SINGLE source of truth for `result.md` write ownership,
/// sunk to octos-agent (the shared dependency of BOTH consumers: the budget
/// checkpoint here, and the cli-side peer-result runtime writer). The
/// judgment itself — sidecar leaf `.result-owner` whose content trims to
/// exactly "peer" ⇒ the runtime must not overwrite `result.md`; anything
/// else (absent, unreadable, other content) is fail-open — lives HERE and
/// nowhere else, so the combined path (peer owns result + dirty wt +
/// MaxIterations) can never clobber the peer's authoritative final version
/// (#27h BLOCKER-2). The cli consumer feeds its fd-anchored read through
/// [`result_md_owner_content_is_peer`], keeping its #1824-safe open path
/// while sharing this one judgment.
pub(super) fn result_md_owned_by_peer(dir: &std::path::Path) -> bool {
    match std::fs::read_to_string(dir.join(".result-owner")) {
        Ok(owner) => result_md_owner_content_is_peer(&owner),
        Err(_) => false, // fail-open: no sidecar ⇒ runtime/checkpoint writes
    }
}

/// #27h-r1 — the ownership JUDGMENT on sidecar CONTENT (the part that must
/// never drift between the two consumers). Public so the cli crate can
/// route its fd-anchored read result through the same single judgment.
pub fn result_md_owner_content_is_peer(owner: &str) -> bool {
    owner.trim() == "peer"
}

pub(super) fn checkpoint_budget_exhaustion(
    workdir: Option<&std::path::Path>,
    stop: &BudgetStop,
    iteration: u32,
) -> Option<String> {
    // Only the ITERATION-cap stop checkpoints: MaxTokens/Shutdown/timeouts
    // have different re-dispatch semantics and stay on the legacy path.
    let BudgetStop::MaxIterations { limit } = stop else {
        return None;
    };
    let dir = workdir?;
    if !dir.join(".git").exists() {
        return None; // not a git worktree — nothing to checkpoint.
    }
    let dirty = git_in(dir, &["status", "--porcelain"])
        .map(|out| !out.is_empty())
        .unwrap_or(false);
    if !dirty {
        return None; // clean wt: no empty commit, no result.md overwrite.
    }
    // ① checkpoint commit (local only — auto-push is forbidden).
    // ② staged result.md FIRST (atomic), so it rides the checkpoint
    // commit below and the tree ends clean.
    let head_before = git_in(dir, &["rev-parse", "--short", "HEAD"]).unwrap_or_default();
    // #27h — respect #27f single-writer ownership BEFORE touching
    // result.md: when the PEER owns the final version (sidecar), the
    // checkpoint writes its staged view to `result.checkpoint.md` (same
    // three-part staged body, atomic tmp+rename) and NEVER overwrites the
    // peer's authoritative `result.md`; the code progress still rides the
    // `git add -A` checkpoint commit below. The sidecar itself is NOT part
    // of the overwrite semantics (it is an ownership marker, not content).
    let peer_owns = result_md_owned_by_peer(dir);
    let result_name = if peer_owns {
        "result.checkpoint.md"
    } else {
        "result.md"
    };
    let body = format!(
        "---\nstatus: budget_exhausted\ncompleted: false\niteration_budget: {limit}\niterations_used: {iteration}\ncheckpoint_commit: {head_before}+\n---\n\n\
         # BUDGET EXHAUSTED — staged result (#27e)\n\n\
         **UNFINISHED**: the turn hit its {limit}-iteration budget cap. The\n\
         work-in-progress is checkpoint-committed on this branch; nothing\n\
         was pushed.\n\n\
         ## Done so far\n- Work-in-progress tree at the checkpoint commit (see `git log -1`)\n\n\
         ## Remaining\n- Preserve this checkpoint and report the unfinished work.\n\
           Resume only under the user's authorized budget; do not automatically reset it.\n\n\
         _Auto-generated by the #27e budget checkpoint; overwrite freely._\n"
    );
    let wrote = write_result_md_named(dir, result_name, &body).is_ok();
    // ① checkpoint commit (local only — auto-push is forbidden). `add -A`
    // so untracked mid-task files (the common peer case) and the staged
    // result.md ride the checkpoint too — `commit -am` alone skips them.
    let _ = git_in(dir, &["add", "-A"]);
    let committed = git_in(
        dir,
        &[
            "commit",
            "-m",
            "wip: budget exhausted (#27e) — checkpointed mid-task",
            "-m",
            &format!("iteration budget {limit} exhausted at iteration {iteration}; work-in-progress preserved for re-dispatch"),
        ],
    )
    .is_some();
    let head = git_in(dir, &["rev-parse", "--short", "HEAD"]).unwrap_or_default();
    if committed || wrote {
        warn!(
            iteration,
            limit,
            committed,
            wrote,
            checkpoint = %head,
            "budget exhausted on a dirty worktree — checkpointed (#27e)"
        );
    }
    // ③ the distinct terminal marker.
    Some(format!("budget_exhausted:{limit}"))
}

#[cfg(test)]
mod budget_checkpoint_tests {
    use super::*;

    /// #27h (BLOCKER-2) — combined path: peer owns result.md + dirty wt +
    /// MaxIterations ⇒ the PEER's authoritative result.md survives verbatim,
    /// the staged view lands in result.checkpoint.md, and the code progress
    /// is still checkpoint-committed (git add -A path unchanged).
    #[test]
    fn peer_owned_result_survives_budget_checkpoint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path();
        init_repo(cwd);
        // Peer's authoritative final version + ownership sidecar.
        let peer_final = "---\nstatus: complete\n---\n\nPEER FINAL — authoritative.\n";
        std::fs::write(cwd.join("result.md"), peer_final).expect("peer result");
        std::fs::write(cwd.join(".result-owner"), "peer").expect("sidecar");
        // Dirty tree (code progress).
        std::fs::write(cwd.join("src.rs"), "fn main() {}").expect("progress");

        let marker =
            checkpoint_budget_exhaustion(Some(cwd), &BudgetStop::MaxIterations { limit: 50 }, 42);

        // Marker unchanged (the distinct terminal signal still fires).
        assert_eq!(marker.as_deref(), Some("budget_exhausted:50"));
        // Peer result.md survives VERBATIM.
        assert_eq!(
            std::fs::read_to_string(cwd.join("result.md")).expect("read"),
            peer_final,
            "peer-owned result.md must not be overwritten (#27h)"
        );
        // Staged view went to result.checkpoint.md with the three parts.
        let staged = std::fs::read_to_string(cwd.join("result.checkpoint.md"))
            .expect("staged checkpoint view");
        assert!(staged.contains("budget_exhausted"));
        assert!(staged.contains("Done so far"));
        assert!(staged.contains("Remaining"));
        // Progress checkpointed: a commit exists and the tree is clean.
        let log = git_in(cwd, &["log", "--oneline"]).expect("log");
        assert!(log.contains("#27e"), "checkpoint commit present: {log}");
        let status = git_in(cwd, &["status", "--porcelain"]).expect("status");
        assert!(
            status.trim().is_empty(),
            "tree clean after checkpoint: {status}"
        );
        // The sidecar is NOT part of the overwrite semantics.
        assert_eq!(
            std::fs::read_to_string(cwd.join(".result-owner")).expect("sidecar"),
            "peer"
        );
    }

    /// #27h — no-sidecar path is byte-identical to 27e: staged view still
    /// lands in result.md itself (fail-open; pinned by the 27e tests too,
    /// asserted here for the combined surface).
    #[test]
    fn no_owner_path_still_writes_result_md() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path();
        init_repo(cwd);
        std::fs::write(cwd.join("wip.txt"), "progress").expect("dirty");
        let marker =
            checkpoint_budget_exhaustion(Some(cwd), &BudgetStop::MaxIterations { limit: 50 }, 10);
        assert_eq!(marker.as_deref(), Some("budget_exhausted:50"));
        assert!(
            cwd.join("result.md").exists(),
            "no sidecar ⇒ staged view lands in result.md (27e behavior)"
        );
        assert!(
            !cwd.join("result.checkpoint.md").exists(),
            "checkpoint mirror only appears when the peer owns result.md"
        );
    }

    fn init_repo(dir: &std::path::Path) {
        for args in [
            vec!["init"],
            vec!["config", "user.name", "t"],
            vec!["config", "user.email", "t@t"],
        ] {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(dir)
                    .args(&args)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        std::fs::write(dir.join("seed.txt"), "seed\n").unwrap();
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["add", "."])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["commit", "-m", "init"])
                .status()
                .unwrap()
                .success()
        );
    }
}
