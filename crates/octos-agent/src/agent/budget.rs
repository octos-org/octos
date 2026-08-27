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
                // Surface the iteration count and a concrete hint about
                // what the user can do next. The previous bare "Reached
                // max iterations." string left users with no signal
                // about what the agent was trying to do, whether any
                // partial work landed, or how to retry. The hint about
                // `run_pipeline` is intentional — the common path into
                // this stop is a manual `web_fetch` / `web_search` loop
                // that should have been a single `run_pipeline` call.
                format!(
                    "The agent did not complete within {limit} iterations. \
                     The task may be too broad for a single turn — try \
                     breaking it into smaller steps, or, if this was a \
                     research/multi-step task, ask the agent to use \
                     `run_pipeline` (deep research) which delegates the \
                     work to specialised sub-agents instead of iterating \
                     one tool call at a time. If this ran as a spawned \
                     sub-agent (e.g. a repo-scale review), re-spawn it with a \
                     higher `max_iterations` — the spawn tool accepts a \
                     `max_iterations` field (up to 300); a broad from-scratch \
                     review typically needs ~100–150."
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
        if iteration >= self.config.max_iterations {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use super::super::{AgentConfig, TokenTracker};

    // ---------- AgentConfig::default ----------

    #[test]
    fn budget_counts_cache_tokens_as_used() {
        // With prompt caching, Anthropic moves most of the prompt out of
        // input_tokens into cache_read/cache_write — the budget must still
        // see the full processed volume or the max_tokens gate fires ~10x
        // late on cache-served loops.
        let usage = TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 4_000,
            cache_write_tokens: 850,
            ..Default::default()
        };
        assert_eq!(budget_tokens_used(&usage), 5_000);

        let uncached = TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            ..Default::default()
        };
        assert_eq!(budget_tokens_used(&uncached), 150);
    }

    #[test]
    fn agent_config_default_values() {
        let cfg = AgentConfig::default();
        assert_eq!(cfg.max_iterations, 50);
        assert_eq!(cfg.max_tokens, None);
        assert_eq!(cfg.max_timeout, Some(Duration::from_secs(1800)));
        assert!(cfg.save_episodes);
        assert_eq!(cfg.tool_timeout_secs, 1800);
        assert!(cfg.worker_prompt.is_none());
    }

    #[test]
    fn default_tool_timeout_matches_max_so_long_running_tools_have_room() {
        // Long-running tools like run_pipeline can legitimately take up to MAX_TOOL_TIMEOUT_SECS.
        // If DEFAULT < MAX, the LLM must remember to pass `timeout_secs` to use the headroom,
        // and forgetting silently caps the call. Keep them equal so the default is the ceiling.
        use super::super::{DEFAULT_TOOL_TIMEOUT_SECS, MAX_TOOL_TIMEOUT_SECS};
        assert_eq!(DEFAULT_TOOL_TIMEOUT_SECS, MAX_TOOL_TIMEOUT_SECS);
        assert_eq!(DEFAULT_TOOL_TIMEOUT_SECS, 1800);
    }

    // ---------- TokenTracker ----------

    #[test]
    fn token_tracker_new_starts_at_zero() {
        let t = TokenTracker::new();
        assert_eq!(t.input_tokens.load(Ordering::Relaxed), 0);
        assert_eq!(t.output_tokens.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn token_tracker_default_starts_at_zero() {
        let t = TokenTracker::default();
        assert_eq!(t.input_tokens.load(Ordering::Relaxed), 0);
        assert_eq!(t.output_tokens.load(Ordering::Relaxed), 0);
    }

    // ---------- BudgetStop::message ----------

    #[test]
    fn budget_stop_shutdown_message() {
        assert_eq!(BudgetStop::Shutdown.message(), "");
    }

    #[test]
    fn budget_stop_max_iterations_message_surfaces_iteration_count_and_hint() {
        // Regression: prior to 2026-05 the bare "Reached max iterations."
        // string was persisted as the assistant reply when the loop hit
        // its iteration cap. That gave the user no signal about what the
        // agent was attempting, no way to recognise the iteration-cap
        // path, and no actionable next step. The fixture below pins the
        // new message contract: (1) the iteration count is named so the
        // user can tell whether the cap was 5 or 500, and (2) a hint
        // about `run_pipeline` is included because the common path into
        // this stop is an LLM that should have called `run_pipeline`
        // once, didn't, and burned its iteration budget on manual
        // `web_fetch` / `web_search` instead.
        let msg = BudgetStop::MaxIterations { limit: 50 }.message();
        assert!(
            msg.contains("50"),
            "expected iteration count '50' in: {msg}"
        );
        assert!(
            msg.to_lowercase().contains("iteration"),
            "expected the word 'iteration' in: {msg}"
        );
        assert!(
            msg.contains("run_pipeline"),
            "expected a hint about 'run_pipeline' (the canonical \
             multi-step research path) in: {msg}"
        );
        // Self-correcting hint: a capped SPAWNED sub-agent's parent must learn
        // it can re-spawn with a higher budget — otherwise the `max_iterations`
        // lever is invisible and the orchestrator keeps starving review lanes.
        assert!(
            msg.contains("max_iterations"),
            "expected the message to point at the `max_iterations` lever: {msg}"
        );
    }

    #[test]
    fn budget_stop_max_iterations_message_uses_actual_limit() {
        // Different iteration caps should surface different numbers.
        let msg = BudgetStop::MaxIterations { limit: 3 }.message();
        assert!(msg.contains("3"), "expected '3' in: {msg}");
        // Sanity: the 50 from the other test must NOT appear when the
        // limit is 3 — guards against a hardcoded constant slipping in.
        assert!(
            !msg.contains("50 iterations") && !msg.contains("50 itr"),
            "limit must not be hardcoded; got: {msg}"
        );
    }

    #[test]
    fn budget_stop_max_tokens_message() {
        let msg = BudgetStop::MaxTokens {
            used: 1000,
            limit: 500,
        }
        .message();
        assert!(
            msg.contains("token") || msg.contains("Token") || msg.contains("TOKEN"),
            "expected 'token' in: {msg}"
        );
        assert!(msg.contains("1000"), "expected '1000' in: {msg}");
        assert!(msg.contains("500"), "expected '500' in: {msg}");
    }

    #[test]
    fn budget_stop_activity_timeout_message() {
        let msg = BudgetStop::ActivityTimeout {
            limit: Duration::from_secs(120),
        }
        .message();
        assert!(
            msg.to_lowercase().contains("activity"),
            "expected 'activity' in: {msg}"
        );
        assert!(
            msg.to_lowercase().contains("timeout"),
            "expected 'timeout' in: {msg}"
        );
    }

    #[test]
    fn budget_stop_idle_progress_timeout_message() {
        let msg = BudgetStop::IdleProgressTimeout {
            limit: Duration::from_secs(120),
        }
        .message();
        assert!(
            msg.to_lowercase().contains("idle"),
            "expected 'idle' in: {msg}"
        );
        assert!(
            msg.to_lowercase().contains("progress"),
            "expected 'progress' in: {msg}"
        );
    }

    async fn test_agent(max_timeout: Option<Duration>) -> super::Agent {
        use super::super::Agent;
        use octos_core::AgentId;
        use octos_llm::{ChatResponse, LlmProvider, ToolSpec};
        use octos_memory::EpisodeStore;
        use std::sync::Arc;

        struct NoopProvider;

        #[async_trait::async_trait]
        impl LlmProvider for NoopProvider {
            async fn chat(
                &self,
                _messages: &[octos_core::Message],
                _tools: &[ToolSpec],
                _config: &octos_llm::ChatConfig,
            ) -> eyre::Result<ChatResponse> {
                eyre::bail!("not used in budget tests")
            }

            fn model_id(&self) -> &str {
                "mock"
            }

            fn provider_name(&self) -> &str {
                "mock"
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
        let provider: Arc<dyn LlmProvider> = Arc::new(NoopProvider);
        let tools = crate::tools::ToolRegistry::new();
        let mut agent = Agent::new(AgentId::new("test-agent"), provider, tools, memory);
        let config = AgentConfig {
            max_timeout,
            ..Default::default()
        };
        agent = agent.with_config(config);
        agent
    }

    #[tokio::test]
    async fn active_progress_allows_runtime_past_activity_timeout() {
        let agent = test_agent(Some(Duration::from_secs(30))).await;
        let activity = super::super::activity::LoopActivityState::new(Instant::now());
        activity.set_last_activity_at(Instant::now() - Duration::from_secs(5));

        let stop = agent.check_budget(
            1,
            Instant::now() - Duration::from_secs(3600),
            &TokenUsage::default(),
            &activity,
        );

        assert!(stop.is_none(), "recent progress should keep the loop alive");
    }

    #[tokio::test]
    async fn stale_progress_trips_activity_timeout_before_idle_timeout() {
        let agent = test_agent(Some(Duration::from_secs(30))).await;
        let activity = super::super::activity::LoopActivityState::new(Instant::now());
        activity.set_last_activity_at(Instant::now() - Duration::from_secs(40));

        let stop = agent.check_budget(
            1,
            Instant::now() - Duration::from_secs(3600),
            &TokenUsage::default(),
            &activity,
        );

        assert!(matches!(
            stop,
            Some(BudgetStop::ActivityTimeout { limit })
                if limit == Duration::from_secs(30)
        ));
    }

    #[tokio::test]
    async fn idle_progress_still_trips_idle_timeout() {
        let agent = test_agent(Some(Duration::from_secs(600))).await;
        let activity = super::super::activity::LoopActivityState::new(Instant::now());
        activity.set_last_activity_at(Instant::now() - Duration::from_secs(301));

        let stop = agent.check_budget(1, Instant::now(), &TokenUsage::default(), &activity);

        assert!(matches!(
            stop,
            Some(BudgetStop::IdleProgressTimeout { limit })
                if limit == Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS)
        ));
    }

    // ---------- ConversationResponse derives ----------

    #[test]
    fn conversation_response_clone_and_debug() {
        use super::super::ConversationResponse;

        let resp = ConversationResponse {
            content: "test".into(),
            reasoning_content: None,
            provider_metadata: None,
            token_usage: octos_core::TokenUsage {
                input_tokens: 10,
                output_tokens: 20,
                ..Default::default()
            },
            estimated_spend_usd: None,
            files_modified: vec![],
            files_to_send: vec![],
            streamed: false,
            messages: vec![],
            tool_results: vec![],
            synthesized_from_spawn_only: false,
            pending_approval: None,
        };
        let cloned = resp.clone();
        assert_eq!(cloned.content, "test");
        assert_eq!(cloned.token_usage.input_tokens, 10);

        // Debug trait works
        let debug = format!("{:?}", cloned);
        assert!(debug.contains("ConversationResponse"));
    }
}

// ─────────────────────────────────────────────────────────────────────────
// #27e (R2) — budget-exhaustion checkpoint: never lose a dirty worktree.
// ─────────────────────────────────────────────────────────────────────────

/// #27e — run `git <args>` in `dir`, returning success + trimmed stdout.
fn git_in(dir: &std::path::Path, args: &[&str]) -> Option<String> {
    std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
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
fn write_result_md_atomic(dir: &std::path::Path, body: &str) -> std::io::Result<()> {
    let final_path = dir.join("result.md");
    let tmp_path = dir.join(".result.md.tmp-27e");
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
/// RED LINES (27e ticket): the default 50 budget is NOT raised (adaptive
/// budgets are out of scope); NOTHING is pushed; a CLEAN worktree produces
/// NO empty commit (the git call is skipped when `status --porcelain` is
/// empty) and no result.md overwrite.
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
    let body = format!(
        "---\nstatus: budget_exhausted\ncompleted: false\niteration_budget: {limit}\niterations_used: {iteration}\ncheckpoint_commit: {head_before}+\n---\n\n\
         # BUDGET EXHAUSTED — staged result (#27e)\n\n\
         **UNFINISHED**: the turn hit its {limit}-iteration budget cap. The\n\
         work-in-progress is checkpoint-committed on this branch; nothing\n\
         was pushed.\n\n\
         ## Done so far\n- Work-in-progress tree at the checkpoint commit (see `git log -1`)\n\n\
         ## Remaining\n- Re-dispatch with a higher `max_iterations` (spawn accepts up to 300) or\n\
           break the task into smaller slices; resume from the checkpoint.\n\n\
         _Auto-generated by the #27e budget checkpoint; overwrite freely._\n"
    );
    let wrote = write_result_md_atomic(dir, &body).is_ok();
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
    Some(format!("budget_exhausted:{}", limit))
}

#[cfg(test)]
mod budget_checkpoint_tests {
    use super::*;

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

    /// #27e — a budget-exhausted turn with a DIRTY worktree checkpoints:
    /// auto-commit exists, result.md is staged with the three essentials
    /// (unfinished / done / remaining) plus the budget_exhausted status,
    /// and the marker names the distinct terminal state.
    #[test]
    fn dirty_worktree_budget_exhaustion_checkpoints() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        // Dirty the tree mid-task.
        std::fs::write(dir.path().join("partial.rs"), "// half-done\n").unwrap();

        let marker = checkpoint_budget_exhaustion(
            Some(dir.path()),
            &BudgetStop::MaxIterations { limit: 50 },
            50,
        )
        .expect("dirty wt checkpoints");

        assert_eq!(marker, "budget_exhausted:50");
        // ① checkpoint commit exists (HEAD moved past init).
        let log = git_in(dir.path(), &["log", "--oneline"]).unwrap();
        assert!(log.contains("#27e"), "checkpoint commit present: {log}");
        // ② nothing uncommitted remains (all work preserved).
        let status = git_in(dir.path(), &["status", "--porcelain"]).unwrap();
        assert!(status.is_empty(), "tree clean after checkpoint: {status}");
        // ③ staged result.md carries the three essentials + status.
        let result = std::fs::read_to_string(dir.path().join("result.md")).unwrap();
        assert!(result.contains("budget_exhausted"), "distinct status");
        assert!(result.contains("UNFINISHED"), "explicitly unfinished");
        assert!(result.contains("## Done so far"), "done section");
        assert!(result.contains("## Remaining"), "remaining section");
        // no leftover tmp file (atomic write completed).
        assert!(!dir.path().join(".result.md.tmp-27e").exists());
    }

    /// #27e — RED LINE: a CLEAN worktree produces NO checkpoint — no empty
    /// commit, no result.md overwrite (the pre-existing behavior holds).
    #[test]
    fn clean_worktree_budget_exhaustion_does_not_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        // Tree stays clean.
        let marker = checkpoint_budget_exhaustion(
            Some(dir.path()),
            &BudgetStop::MaxIterations { limit: 50 },
            50,
        );
        assert!(marker.is_none(), "clean wt must not checkpoint");
        assert!(
            !dir.path().join("result.md").exists(),
            "no result.md written"
        );
        let log = git_in(dir.path(), &["log", "--oneline"]).unwrap();
        assert_eq!(log.lines().count(), 1, "no empty commit: {log}");
    }
}
