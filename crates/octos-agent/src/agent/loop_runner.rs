//! Main agent loop: process_message and run_task orchestration.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use std::{collections::HashMap, collections::HashSet, collections::VecDeque};

use eyre::Result;
use octos_core::{Message, MessageRole, Task, TaskResult, TokenUsage};
use octos_llm::{ChatConfig, ChatResponse, StopReason};
use octos_memory::{Episode, EpisodeOutcome};
use tracing::{Instrument, info, info_span, warn};

use super::activity::{ActivityTrackingReporter, LoopActivityState};
use super::budget::BudgetStop;
use super::loop_compaction::{prepare_conversation_messages, prepare_task_messages};
use super::loop_state::{LoopDecision, LoopRetryState, SHELL_SPIRAL_VARIANT};
use super::message_repair::sanitize_tool_call_id;
use super::turn_state::{LoopRetryReason, LoopTurnState, attach_partial_usage};
use super::verifier::{TurnLedger, ledger_entry_from_tool_result};
use super::{Agent, ConversationResponse, TASK_REPORTER, TokenTracker};
use crate::harness_errors::HarnessError;
use crate::harness_events::write_event_to_sink;
use crate::hooks::{HookEvent, HookPayload, HookResult};
use crate::loop_detect::LoopDetector;
use crate::progress::ProgressEvent;
use crate::prompt_context::PromptContextPhase;
use crate::session::SessionLimits;
use crate::tools::{TURN_ATTACHMENT_CTX, TurnAttachmentContext};

const MAX_PARALLEL_TOOL_CALLS_PER_BATCH: usize = 8;
const MAX_TOKENS_CONTINUATION_LIMIT: usize = 2;
const MAX_TOKENS_CONTINUATION_PROMPT: &str = "Your output was truncated at the token limit. Continue directly from where you stopped. Do not repeat or summarize what you already wrote.";
const SHELL_RETRY_RECOVERY_THRESHOLD: usize = 4;

/// Prepended to the user content on a live video-call turn so the model treats
/// the attached image as the user's real-time camera view rather than a file
/// they uploaded.
const VIDEO_CALL_NOTE: &str =
    "[Live video call — the attached image is the user's current camera frame.]";

/// Compose the user message content for a turn.
///
/// - `is_video_call` (the turn carries both an audio attachment AND an image)
///   prepends [`VIDEO_CALL_NOTE`] so a real-time camera frame isn't mistaken
///   for an uploaded file — applied whether or not the spoken turn was
///   transcribed into non-empty text.
/// - With empty `user_content` and image media present (but not a video call),
///   the legacy `[User sent an image]` placeholder is kept.
/// - Any per-turn `prompt_summary` is appended, mirroring the previous
///   behaviour.
///
/// Pure (no task-locals) so it is unit-testable in isolation.
fn compose_turn_user_content(
    user_content: &str,
    has_image: bool,
    is_video_call: bool,
    prompt_summary: Option<&str>,
) -> String {
    let base_content = if user_content.is_empty() {
        if is_video_call {
            VIDEO_CALL_NOTE.to_string()
        } else if has_image {
            "[User sent an image]".to_string()
        } else {
            String::new()
        }
    } else if is_video_call {
        format!("{VIDEO_CALL_NOTE}\n\n{user_content}")
    } else {
        user_content.to_string()
    };

    match prompt_summary {
        Some(summary) if base_content.trim().is_empty() => summary.to_string(),
        Some(summary) => format!("{base_content}\n\n{summary}"),
        None => base_content,
    }
}

/// Audit Gap-8 helper: consult the workspace-contract layer at EndTurn time
/// and return a human-readable summary of failing validators when the
/// contract is NOT ready. Returns `None` when the workspace has no
/// policy-managed repos under `working_dir` (today's silent-success path).
///
/// This is the harness-side mirror of the LLM-callable
/// `check_workspace_contract` tool — same source of truth
/// (`inspect_workspace_contracts`), no parallel framework. Errors from the
/// underlying inspector are swallowed with a warning so a transient git
/// failure (e.g. corrupt `.git` directory) cannot block an otherwise
/// successful task; the previous behaviour is preserved on inspector error.
fn inspect_workspace_contract_failures(working_dir: &std::path::Path) -> Option<String> {
    let contracts = match crate::workspace_git::inspect_workspace_contracts(working_dir) {
        Ok(contracts) => contracts,
        Err(err) => {
            warn!(
                workspace_root = %working_dir.display(),
                error = %err,
                "workspace contract inspector failed at EndTurn; treating as no-policy"
            );
            return None;
        }
    };

    // Only fail on policy-managed repos that aren't ready.
    let failing: Vec<_> = contracts
        .iter()
        .filter(|status| status.policy_managed && !status.ready)
        .collect();
    if failing.is_empty() {
        return None;
    }

    let mut lines = Vec::with_capacity(failing.len() * 2);
    // Lowercase "workspace contract" so the message matches the same
    // grep predicate used by the existing spawn-task contract failure
    // assertions (`error.contains("workspace contract")` in spawn.rs).
    lines.push(format!(
        "workspace contract not ready for {} repo(s):",
        failing.len()
    ));
    for status in failing {
        lines.push(format!("- {} (kind={})", status.repo_label, status.kind));
        if let Some(ref error) = status.error {
            lines.push(format!("    error: {error}"));
        }
        for check in &status.completion_checks {
            if !check.passed {
                let reason = check.reason.as_deref().unwrap_or("(no reason given)");
                lines.push(format!("    completion failed: {} — {reason}", check.spec));
            }
        }
        for check in &status.turn_end_checks {
            if !check.passed {
                let reason = check.reason.as_deref().unwrap_or("(no reason given)");
                lines.push(format!("    turn_end failed: {} — {reason}", check.spec));
            }
        }
        for missing in status.artifacts.iter().filter(|a| !a.present) {
            lines.push(format!(
                "    artifact missing: {} (pattern={})",
                missing.name, missing.pattern
            ));
        }
    }
    Some(lines.join("\n"))
}

fn split_tool_calls(
    tool_calls: &[octos_core::ToolCall],
    batch_size: usize,
) -> Vec<&[octos_core::ToolCall]> {
    debug_assert!(batch_size > 0);
    tool_calls.chunks(batch_size).collect()
}

/// M8.5 tier 1 safety helper: collect the set of `tool_call_id`s that are
/// currently in an unresolved state (i.e. an assistant tool call whose
/// matching [`MessageRole::Tool`] reply has not landed yet). Those IDs are
/// passed to the tier-1 prune pass as "protected" so we never drop a tool
/// result that a pending retry/contract-gate handler still needs.
///
/// Works purely off the message list so it also covers contract-gated
/// artifacts that are referenced by message indices — content-clearing
/// preserves indices, but full pruning would not, so the prune pass
/// explicitly skips these.
fn collect_protected_tool_call_ids(messages: &[Message]) -> Vec<String> {
    let mut requested: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut answered: std::collections::HashSet<String> = std::collections::HashSet::new();
    for msg in messages {
        match msg.role {
            MessageRole::Assistant => {
                if let Some(ref calls) = msg.tool_calls {
                    for call in calls {
                        requested.insert(call.id.clone());
                    }
                }
            }
            MessageRole::Tool => {
                if let Some(ref id) = msg.tool_call_id {
                    answered.insert(id.clone());
                }
            }
            _ => {}
        }
    }
    requested.difference(&answered).cloned().collect()
}

/// M8.5 tier 2 helper: returns a `ChatConfig` with the agent's tier-2
/// `context_management` payload attached when the active provider is
/// Anthropic-flavoured.  Returns a clone with the field left as-is in every
/// other case so non-Anthropic providers never see the Anthropic-only
/// header.
fn with_tier2_context_management(config: &ChatConfig, agent: &Agent) -> ChatConfig {
    let Some(payload) = agent.build_tier2_context_management() else {
        return config.clone();
    };
    let mut out = config.clone();
    out.context_management = Some(payload);
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShellRetryRecoveryKind {
    DiffLikeSuccess,
    UsefulSuccess,
    ValidationSuccess,
    RetryLimit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShellRetryRecovery {
    pub(crate) kind: ShellRetryRecoveryKind,
    pub(crate) content: String,
}

/// Coarse-grained control-flow hint returned by
/// [`Agent::handle_loop_error_with_dispatch`]: the caller acts on this
/// without having to re-match on [`LoopDecision`] at every error site.
///
/// Semantics:
///   * `Retry` — the retry layer decided the loop should continue
///     (optionally after compaction, which is performed inline for
///     `CompactAndRetry`). The caller should `continue` its outer loop.
///   * `Bail` — the error is structural, non-retryable, or the bucket
///     for the variant has been exhausted. The caller must surface
///     `Err(report)` to its own caller.
///
/// The in-band `RotateAndRetry` arm degrades to `Bail` in this release
/// because no in-band credential-rotation hook is wired on `Agent` yet;
/// lane rotation is already handled by the outer provider chain
/// (`RetryProvider` → `AdaptiveRouter`) one layer down, so surfacing
/// the error is safe — the next inbound message starts a fresh retry
/// state anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoopErrorAction {
    /// Continue the outer agent loop with the next iteration.
    Retry,
    /// Abort the outer agent loop and surface `Err(report)`.
    Bail,
}

/// Review A F-015 RAII guard. Loads a `LoopRetryState` from an optional
/// shared `Arc<Mutex<...>>` at construction and writes back on drop so
/// bucket counters persist across `process_message` / `run_task` calls
/// for sessions that attach a persistent retry-state handle.
///
/// The loop body accesses the owned `state` field via `Deref`/`DerefMut`
/// so existing code keeps its `&mut retry_state` call pattern.
///
/// Sessions that do not attach a handle see the legacy reset-per-turn
/// behaviour — the guard just owns a fresh `LoopRetryState` and writes
/// nowhere on drop.
struct PersistentRetryStateGuard {
    state: super::loop_state::LoopRetryState,
    handle: Option<Arc<std::sync::Mutex<super::loop_state::LoopRetryState>>>,
}

impl PersistentRetryStateGuard {
    fn new(handle: Option<Arc<std::sync::Mutex<super::loop_state::LoopRetryState>>>) -> Self {
        let state = handle
            .as_ref()
            .map(|h| h.lock().unwrap_or_else(|e| e.into_inner()).clone())
            .unwrap_or_default();
        Self { state, handle }
    }
}

impl std::ops::Deref for PersistentRetryStateGuard {
    type Target = super::loop_state::LoopRetryState;
    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

impl std::ops::DerefMut for PersistentRetryStateGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.state
    }
}

impl Drop for PersistentRetryStateGuard {
    fn drop(&mut self) {
        if let Some(handle) = &self.handle {
            let mut locked = handle.lock().unwrap_or_else(|e| e.into_inner());
            *locked = self.state.clone();
        }
    }
}

impl Agent {
    /// Classify a raw error escaping the agent loop into a `HarnessError`,
    /// increment the `octos_loop_error_total{variant, recovery}` counter, and
    /// emit a structured error event via the local harness event sink (if
    /// one is attached). Returns the classified error so the caller can log
    /// it or convert it into an `eyre::Report` for the caller's contract.
    ///
    /// Invariant (#488): every raw `eyre::Report` that would otherwise bubble
    /// out of the agent loop must be routed through this classifier.
    pub(crate) fn classify_loop_error(
        &self,
        report: &eyre::Report,
        tool_name: Option<&str>,
    ) -> HarnessError {
        let classified = HarnessError::classify_report(report, tool_name);
        classified.record_metric();

        if let Some(sink) = self.harness_event_sink.as_deref() {
            let (session_id, task_id) = self.harness_error_context();
            let event = classified.to_event(
                session_id, task_id, /* workflow */ None, /* phase */ None,
            );
            if let Err(error) = write_event_to_sink(sink, &event) {
                tracing::debug!(error = %error, "failed to write harness error event to sink");
            }
        }

        tracing::warn!(
            variant = classified.variant_name(),
            recovery = %classified.recovery_hint(),
            error = %report,
            "harness error classified"
        );
        classified
    }

    fn harness_error_context(&self) -> (String, String) {
        // The agent loop itself does not own a task_id — those are assigned
        // per-spawn in `task_supervisor`. Use the registered sink context
        // (written by `HarnessEventSink::new`) when available; fall back to
        // stable placeholders so the event still validates.
        if let Some(sink) = self.harness_event_sink.as_deref() {
            if let Some(ctx) = crate::harness_events::lookup_event_sink_context(sink) {
                return (ctx.session_id, ctx.task_id);
            }
        }
        let session_id = self
            .hook_ctx()
            .and_then(|ctx| ctx.session_id)
            .unwrap_or_else(|| "unknown".to_string());
        (session_id, "agent".to_string())
    }

    /// Shell-spiral dispatch (M6.2, issue #489). Routes the existing shell
    /// retry recovery through the [`LoopRetryState`] state machine so
    /// operators see one coherent retry ledger and the spiral bucket is
    /// bounded. Returns the recovered shell output when the detector finds a
    /// stable response, or `None` when no spiral is in progress.
    ///
    /// Behavior preserved from the pre-M6.2 free-standing
    /// `recover_shell_retry` call site: identical detection input produces
    /// identical content bytes — the only new side effects are
    ///   1. an increment on `octos_loop_retry_total{variant="shell_spiral",decision="escalate"}`, and
    ///   2. a `HarnessEventPayload::Retry` event written to the harness sink.
    pub(crate) fn dispatch_shell_retry_recovery(
        &self,
        messages: &[Message],
        retry_state: &mut LoopRetryState,
        iteration: u32,
    ) -> Option<ShellSpiralOutcome> {
        // Fix #1 (2026-05-10, codex round 2): the spiral detector must be
        // INTRA-TURN. Two prior bugs:
        //   (a) the unconditional dispatch scanned the entire session's
        //       message history, so once any past turn accumulated a
        //       4-shell streak with failures, every subsequent turn was
        //       force-ended regardless of its tool;
        //   (b) gating only on `latest_completed_tool_name == shell`
        //       would (i) miss multi-tool batches like
        //       `[shell, read_file]` where the trailing Tool message is
        //       `read_file`, AND (ii) trip on a single fresh shell call
        //       in a new user turn that happens to come AFTER stale
        //       history.
        //
        // Restrict the scan to the slice from the most recent
        // `MessageRole::User` onward (the current user turn) and gate on
        // "did the latest completed Tool BATCH contain shell". With both
        // in place the detector matches its intent: the LLM is currently
        // spiraling on shell within this turn.
        let window_start = current_user_turn_start(messages);
        let window = &messages[window_start..];
        if !latest_tool_batch_contains(window, "shell") {
            return None;
        }
        let recovery = recover_shell_retry(window, SHELL_RETRY_RECOVERY_THRESHOLD)?;
        // #1656: a firing spiral IS a loop detection — mark the two-stage
        // dedup flag so a generic loop detection later in the SAME turn is
        // treated as the second fire (terminal) instead of restarting the
        // warn-then-terminate ladder with no memory of this one.
        self.mark_loop_detected_recently();
        let decision = retry_state.observe_shell_spiral();
        tracing::warn!(
            recovery_kind = ?recovery.kind,
            decision = %decision,
            "shell spiral detected; routing through LoopRetryState"
        );

        if let Some(sink) = self.harness_event_sink.as_deref() {
            let (session_id, task_id) = self.harness_error_context();
            let event = retry_state.emit_event(
                SHELL_SPIRAL_VARIANT,
                decision,
                session_id,
                task_id,
                /* workflow */ None,
                /* phase */ None,
                Some(iteration),
            );
            if let Err(error) = write_event_to_sink(sink, &event) {
                tracing::debug!(error = %error, "failed to write shell-spiral retry event");
            }
        }
        Some(ShellSpiralOutcome { recovery, decision })
    }

    /// Classify an error escaping the loop and drive it through the
    /// [`LoopRetryState`] state machine (M6.2). Returns the bucketed
    /// [`LoopDecision`] for the caller to act on. Also emits a typed
    /// `HarnessEventPayload::Retry` event to the harness sink.
    ///
    /// This does NOT replace [`Self::classify_loop_error`]: the error event
    /// still gets emitted, metrics still update, and the caller still owns
    /// the decision of whether to return `Err(report)` after the state
    /// machine has been driven.
    pub(crate) fn dispatch_loop_error(
        &self,
        error: &HarnessError,
        retry_state: &mut LoopRetryState,
        iteration: u32,
    ) -> LoopDecision {
        let decision = retry_state.observe(error);
        if let Some(sink) = self.harness_event_sink.as_deref() {
            let (session_id, task_id) = self.harness_error_context();
            let event = retry_state.emit_event(
                error.variant_name(),
                decision,
                session_id,
                task_id,
                /* workflow */ None,
                /* phase */ None,
                Some(iteration),
            );
            if let Err(error) = write_event_to_sink(sink, &event) {
                tracing::debug!(error = %error, "failed to write harness retry event");
            }
        }
        decision
    }

    /// Run the harness error classifier, dispatch the classified error
    /// through the `LoopRetryState` bucket machine, and return a coarse
    /// [`LoopErrorAction`] the caller can act on with a plain
    /// `match action { Retry => continue, Bail => return Err(e) }`.
    ///
    /// `CompactAndRetry` is handled in-band: the method calls
    /// [`Self::maybe_run_turn_compaction`] before returning `Retry` so the
    /// caller does not have to thread compaction state across error sites.
    ///
    /// This is the wiring seam added for Review A F-001. Prior to this
    /// patch every error site in `process_message` / `run_task` classified
    /// errors for metrics and then bailed with `Err(e)` unconditionally;
    /// every `LoopDecision` other than `Escalate` was dead. Now every
    /// decision arm is reachable.
    fn handle_loop_error_with_dispatch(
        &self,
        error: &eyre::Report,
        retry_state: &mut LoopRetryState,
        iteration: u32,
        messages: &mut Vec<Message>,
    ) -> LoopErrorAction {
        let classified = self.classify_loop_error(error, None);
        let decision = self.dispatch_loop_error(&classified, retry_state, iteration);
        match decision {
            LoopDecision::Continue => {
                tracing::info!(
                    variant = classified.variant_name(),
                    iteration,
                    "loop retry: continuing after transient error"
                );
                LoopErrorAction::Retry
            }
            LoopDecision::CompactAndRetry => {
                tracing::info!(
                    variant = classified.variant_name(),
                    iteration,
                    "loop retry: compacting context before retry"
                );
                if let Err(error) = self.maybe_run_turn_compaction(messages, iteration) {
                    tracing::warn!(
                        error = %error,
                        "loop retry: compaction preservation failed; bailing"
                    );
                    return LoopErrorAction::Bail;
                }
                self.prepare_prompt_with_context_manager(
                    messages,
                    PromptContextPhase::Retry,
                    iteration,
                );
                LoopErrorAction::Retry
            }
            LoopDecision::RotateAndRetry => {
                // No in-band credential rotation hook on Agent in this
                // release — lane rotation is already owned by the outer
                // provider chain. Degrade to Bail so the caller surfaces
                // the error rather than looping on a sick lane.
                tracing::warn!(
                    variant = classified.variant_name(),
                    iteration,
                    "loop retry: rotate_and_retry requested but no hook wired; bailing"
                );
                LoopErrorAction::Bail
            }
            LoopDecision::Escalate => {
                tracing::warn!(
                    variant = classified.variant_name(),
                    iteration,
                    "loop retry: escalating non-recoverable error"
                );
                LoopErrorAction::Bail
            }
            LoopDecision::Exhausted => {
                tracing::error!(
                    variant = classified.variant_name(),
                    iteration,
                    "loop retry: bucket exhausted, bailing"
                );
                LoopErrorAction::Bail
            }
            LoopDecision::Grace => {
                // Grace decisions come from observe_budget_exhaustion, not
                // from observe(&HarnessError). Treat defensively as Retry
                // so the grace path behaves consistently if it is ever
                // reached via this code path (it isn't today).
                LoopErrorAction::Retry
            }
        }
    }

    /// Task 8 — FailFast foreground-LLM-call failure handling.
    ///
    /// Called ONLY from the foreground LLM call sites (not from tool/verifier
    /// dispatch). When the loop runs under
    /// [`octos_llm::LlmCallPolicy::FailFast`] and a foreground LLM call fails,
    /// this:
    ///   1. Excludes hook-deny errors (`"LLM call denied by hook"`): returns
    ///      `false` WITHOUT emitting a [`crate::TurnFailure`] so the caller
    ///      falls through to the existing dispatch path and the permission
    ///      behaviour is preserved byte-for-byte.
    ///   2. Otherwise runs [`Self::classify_loop_error`] EXACTLY ONCE (records
    ///      the metric + harness event; honours the "all escaping Reports go
    ///      through the classifier" invariant), emits a single
    ///      [`crate::TurnFailure::LlmError`] on the voice failure sink (if one
    ///      is attached), and returns `true` so the caller bails with the
    ///      ORIGINAL `report` (NOT through `handle_loop_error_with_dispatch`).
    ///
    /// Returns `false` under Normal policy so non-FailFast behaviour — and the
    /// entire `handle_loop_error_with_dispatch` path — is unchanged.
    fn failfast_llm_bail(&self, report: &eyre::Report) -> bool {
        if octos_llm::current_llm_call_policy() != octos_llm::LlmCallPolicy::FailFast {
            return false;
        }
        // Exclude hook-deny: preserve existing permission behaviour (no
        // TurnFailure, fall through to the caller's dispatch path).
        if report.to_string().starts_with("LLM call denied by hook") {
            return false;
        }
        // Classify exactly once (keeps metric + harness-event side effects and
        // the #488 invariant). The classified error is carried by the voice
        // projection; the original `report` still bubbles out to the caller.
        let classified = self.classify_loop_error(report, None);
        if let Some(sink) = &self.voice_failure_sink {
            let _ = sink.send(crate::TurnFailure::LlmError {
                error: classified,
                raw_detail: report.to_string(),
            });
        }
        true
    }

    /// Budget grace-call dispatch (M6.2). When the loop hits a hard iteration
    /// or token budget, this asks the retry state machine whether to grant
    /// one free iteration past budget. Only `MaxIterations` and `MaxTokens`
    /// stops are eligible — `Shutdown`, `ActivityTimeout`, and
    /// `IdleProgressTimeout` are always hard stops so stalled loops and
    /// operator shutdowns terminate immediately.
    ///
    /// Returns `true` iff a grace call was granted; the caller should skip
    /// its budget-stop return path and proceed with one more iteration.
    pub(super) fn try_budget_grace_call(
        &self,
        stop: &BudgetStop,
        retry_state: &mut LoopRetryState,
        iteration: u32,
    ) -> bool {
        if !matches!(
            stop,
            BudgetStop::MaxIterations { .. } | BudgetStop::MaxTokens { .. }
        ) {
            return false;
        }
        let decision = retry_state.observe_budget_exhaustion();
        if let Some(sink) = self.harness_event_sink.as_deref() {
            let (session_id, task_id) = self.harness_error_context();
            let event = retry_state.emit_event(
                "budget_exhaustion",
                decision,
                session_id,
                task_id,
                /* workflow */ None,
                /* phase */ None,
                Some(iteration),
            );
            if let Err(error) = write_event_to_sink(sink, &event) {
                tracing::debug!(error = %error, "failed to write budget-grace retry event");
            }
        }
        match decision {
            LoopDecision::Grace => {
                tracing::warn!(
                    iteration,
                    "budget exhausted; granting one grace call via LoopRetryState"
                );
                true
            }
            _ => false,
        }
    }

    fn enforce_session_limits_on_tool_calls(
        &self,
        response: &ChatResponse,
    ) -> (ChatResponse, Vec<Message>) {
        let Some(limits) = self.session_limits.as_ref() else {
            return (response.clone(), Vec::new());
        };
        if response.tool_calls.is_empty() {
            return (response.clone(), Vec::new());
        }

        let mut usage = self.session_usage.lock().unwrap_or_else(|e| e.into_inner());
        let round_allowed = limits
            .max_tool_rounds
            .is_none_or(|max_rounds| usage.tool_rounds < max_rounds);

        let mut allowed_calls = Vec::new();
        let mut blocked_messages = Vec::new();
        let mut recorded_round = false;

        for tool_call in &response.tool_calls {
            if !round_allowed {
                blocked_messages.push(session_limit_message(
                    tool_call,
                    format!(
                        "[SESSION LIMIT] Tool '{}' exceeded the workflow tool-round budget. Do not retry this tool in this run.",
                        tool_call.name
                    ),
                ));
                continue;
            }

            let call_allowed = check_per_tool_limit(&usage, tool_call.name.as_str(), limits);
            if call_allowed {
                if !recorded_round {
                    usage.record_tool_round();
                    recorded_round = true;
                }
                usage.record_tool_call(&tool_call.name);
                allowed_calls.push(tool_call.clone());
            } else {
                let max_calls = limits
                    .per_tool_limits
                    .get(&tool_call.name)
                    .copied()
                    .unwrap_or_default();
                blocked_messages.push(session_limit_message(
                    tool_call,
                    format!(
                        "[SESSION LIMIT] Tool '{}' exceeded its workflow limit (max {}). Do not retry this tool in this run.",
                        tool_call.name, max_calls
                    ),
                ));
            }
        }

        let mut limited = response.clone();
        limited.tool_calls = allowed_calls;
        (limited, blocked_messages)
    }

    /// Build a `ChatConfig` with optional `chat_max_tokens` override from `AgentConfig`.
    fn chat_config(&self) -> ChatConfig {
        let mut c = ChatConfig::default();
        if let Some(max) = self.config.chat_max_tokens {
            c.max_tokens = Some(max);
        }
        c.reasoning_effort = self.config.reasoning_effort;
        c
    }

    /// Decide what to surface when the loop detector fires.
    ///
    /// First fire in a session-burst: returns the warning text and marks the
    /// session as having warned. Subsequent fires within the same burst
    /// (before the next `process_message` reset) return a terminal error so
    /// the loop cannot keep emitting identical noise to the user.
    pub(super) fn dedup_loop_warning(&self, warning: String) -> Result<String> {
        if self.is_loop_detected_recently() {
            return Err(eyre::eyre!(
                "agent loop got stuck — please rephrase or simplify your request"
            ));
        }
        self.mark_loop_detected_recently();
        Ok(warning)
    }

    /// Drain the per-turn steer buffer (mid-turn injected user inputs) into
    /// the live conversation.
    ///
    /// Codex parity: `run_turn` drains `TurnState.pending_input` at the TOP
    /// of each loop iteration, before building the next model request
    /// (codex-rs `core/src/session/turn.rs:225-233`), and records each item
    /// as a plain `role: user` message with NO wrapper text
    /// (`record_user_prompt_and_emit_turn_item`). FIFO order is the
    /// buffer's append order (`split_off(0)` semantics).
    ///
    /// Persistence ownership: when a drained-callback is registered the
    /// HOST persists each steer row (and emits its standard persisted
    /// user-message event) at drain time, so the rows stay OUT of
    /// `turn_output_log` — otherwise the end-of-turn persist pass would
    /// write them a second time. Without a callback (chat/gateway paths)
    /// the rows ride the normal end-of-turn persistence via the log.
    ///
    /// No-op without a configured buffer — pre-steer loops are
    /// byte-identical.
    async fn drain_pending_steer_input(
        &self,
        messages: &mut Vec<Message>,
        turn_output_log: &mut Vec<Message>,
    ) {
        let Some(buffer) = self.steer_buffer.as_ref() else {
            return;
        };
        let drained = buffer.drain();
        if drained.is_empty() {
            return;
        }
        tracing::info!(
            count = drained.len(),
            "draining mid-turn steer input into the conversation before the next LLM call"
        );
        for text in &drained {
            let message = Message::user(text.clone());
            messages.push(message.clone());
            if self.steer_drained_callback.is_none() {
                turn_output_log.push(message);
            }
        }
        if let Some(callback) = self.steer_drained_callback.as_ref() {
            callback(drained).await;
        }
    }

    /// Whether a mid-turn steer input is pending (codex `has_pending_input`,
    /// `turn.rs:304-318`). Read in the EndTurn arm so
    /// `needs_follow_up = model_wants_more || buffer_nonempty` — a steer
    /// landing after the model's final answer forces one more round.
    fn steer_input_pending(&self) -> bool {
        self.steer_buffer
            .as_ref()
            .is_some_and(|buffer| !buffer.is_empty())
    }

    /// Process a single message in conversation mode (chat/gateway).
    /// Takes the user's message, conversation history, and optional media paths.
    pub async fn process_message(
        &self,
        user_content: &str,
        history: &[Message],
        media: Vec<String>,
    ) -> Result<ConversationResponse> {
        self.process_message_inner(
            user_content,
            history,
            media,
            TurnAttachmentContext::default(),
            None,
        )
        .await
    }

    pub async fn process_message_with_attachments(
        &self,
        user_content: &str,
        history: &[Message],
        media: Vec<String>,
        attachments: TurnAttachmentContext,
    ) -> Result<ConversationResponse> {
        self.process_message_inner(user_content, history, media, attachments, None)
            .await
    }

    /// Like `process_message`, but updates a `TokenTracker` in real-time after each LLM call.
    /// Used by the gateway status indicator to show live token counts.
    pub async fn process_message_tracked(
        &self,
        user_content: &str,
        history: &[Message],
        media: Vec<String>,
        tracker: &TokenTracker,
    ) -> Result<ConversationResponse> {
        self.process_message_inner(
            user_content,
            history,
            media,
            TurnAttachmentContext::default(),
            Some(tracker),
        )
        .await
    }

    pub async fn process_message_tracked_with_attachments(
        &self,
        user_content: &str,
        history: &[Message],
        media: Vec<String>,
        attachments: TurnAttachmentContext,
        tracker: std::sync::Arc<TokenTracker>,
    ) -> Result<ConversationResponse> {
        self.process_message_inner(
            user_content,
            history,
            media,
            attachments,
            Some(tracker.as_ref()),
        )
        .await
    }

    async fn process_message_inner(
        &self,
        user_content: &str,
        history: &[Message],
        media: Vec<String>,
        attachments: TurnAttachmentContext,
        tracker: Option<&TokenTracker>,
    ) -> Result<ConversationResponse> {
        let activity = Arc::new(LoopActivityState::new(Instant::now()));
        let activity_reporter = Arc::new(ActivityTrackingReporter::new(
            activity.clone(),
            self.reporter(),
        ));
        TURN_ATTACHMENT_CTX
            .scope(
                attachments,
                TASK_REPORTER.scope(activity_reporter, async move {
                // Reset per-run flags
                self.tools.reset_spawn_only_invoked();
                self.reset_loop_detected_recently();

                // Refresh provider-backed prompt segments (e.g. the memory
                // block when MEMORY.md changed on disk) before composing.
                // No-op unless providers are registered; providers keep the
                // unchanged path to a single stat.
                self.refresh_prompt_segments().await;

                // Build the system prompt via the shared helper in
                // execution.rs so conversation + task loops compose the same
                // prompt. This is where realtime sensor summary gets appended
                // once per turn (bounded by `sensor_budget_tokens`).
                let mut messages = vec![Message {
                    role: MessageRole::System,
                    content: super::execution::compose_system_prompt(self),
                    media: vec![],
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: None,
                    client_message_id: None,
                    thread_id: None,
                    timestamp: chrono::Utc::now(),
                }];

                // #1587: session-start conversational episodic recall.
                // Recall on an EMPTY-history turn — normally the first turn
                // of a conversation — so the cost (one embed) is paid once
                // per conversation, NOT per turn, and the contamination
                // surface is a single injection. Inherits the guard from the
                // shared helper (embedder-only, 0.55 floor); no-op without an
                // embedder or on empty input.
                //
                // "Empty history" is the trigger, not a strict session
                // counter: a rare speculative-interrupt retry can re-invoke
                // with the empty pre-primary snapshot and recall again. That
                // is harmless — each turn builds a FRESH prompt, so a repeat
                // adds one embed on that path and never accumulates or
                // duplicates within a prompt. A "recalled once" marker was
                // deliberately NOT added: if the agent were reused across a
                // `/new` fork it would wrongly SUPPRESS recall on a genuine
                // new conversation, and missing recall is worse than a rare
                // extra embed on an advisory feature.
                if history.is_empty() && !user_content.trim().is_empty() {
                    let default_cwd = std::path::PathBuf::from(".");
                    let cwd = self.tools.workspace_root().unwrap_or(default_cwd.as_path());
                    if let Some(recall) =
                        self.recall_relevant_episodes(user_content, cwd, true).await
                    {
                        messages.push(recall);
                    }
                }

                messages.extend_from_slice(history);

                // A turn carrying BOTH an audio attachment (a spoken/voice turn)
                // and an image is a live video call: the image is the user's
                // current camera frame, not an uploaded file. Tell the model so
                // it treats the picture as a real-time view. `had_audio` comes
                // from the per-turn attachment context (the audio is already
                // transcribed into `user_content` by this point), and image
                // detection looks at the outgoing `media`.
                let has_image = media.iter().any(|p| octos_llm::vision::is_image(p));
                // Live-video is an EXPLICIT per-turn signal carried on the turn
                // context (set by the ingress from `inbound.metadata.live_video`),
                // not inferred from attachments: a spoken note plus an uploaded
                // image is not a camera frame, and the AppUI voice path strips
                // the audio attachment before this point — so an `audio && image`
                // heuristic both mis-fires and misses the real voice+image path.
                // Only treat the image as a live camera frame when the client said so.
                let live_video = TURN_ATTACHMENT_CTX
                    .try_with(|ctx| ctx.live_video)
                    .ok()
                    .unwrap_or(false);
                let summary = TURN_ATTACHMENT_CTX
                    .try_with(|ctx| ctx.prompt_summary.clone())
                    .ok()
                    .flatten();
                let content = compose_turn_user_content(
                    user_content,
                    has_image,
                    live_video && has_image,
                    summary.as_deref(),
                );

                let current_user = Message {
                    role: MessageRole::User,
                    content,
                    media,
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: None,
                    client_message_id: None,
                    thread_id: None,
                    timestamp: chrono::Utc::now(),
                };
                messages.push(current_user.clone());

                // NEW-16 (codex design): append-only per-turn output log.
                //
                // The persisted `ConversationResponse.messages` MUST NOT be
                // derived from the LLM prompt buffer (`messages`) by slicing
                // at `1 + history.len()`. That buffer is mutated during the
                // loop by `prepare_conversation_messages` (which calls
                // `repair_message_order`) and by the AppUI context-window
                // bridge in `ui_protocol.rs`. After mutation, OLD rows from
                // prior turns can end up past the stale boundary and get
                // returned as "new", which causes re-persistence and the
                // 7x duplicate-content drag-forward seen in soak captures
                // (mini3 Yuan-dynasty content, 2026-05-23).
                //
                // Instead, we build an append-only log of just the rows we
                // genuinely produce in THIS turn (current User, assistant
                // replies + tool results from `handle_tool_use`, synthetic
                // loop-detector rows, and any terminal/synthesised assistant
                // row a return site adds). The log is never read back from
                // — only pushed to — so no mutation pass can shift OLD rows
                // into it.
                let mut turn_output_log: Vec<Message> = vec![current_user];

                let config = self.chat_config();
                let mut files_modified = Vec::new();
                let mut files_to_send = Vec::new();
                // Accumulate the structured side-channel metadata that tools
                // surface during this turn (today: `node_costs` from
                // `run_pipeline`). Threaded into every `ConversationResponse`
                // built below so the session actor can plumb it into the SSE
                // `done` event for the W1.G4 cost panel.
                let mut tool_structured_metadata: Vec<(String, serde_json::Value)> = Vec::new();
                let mut turn = LoopTurnState::new(Instant::now());
                // M6.2: per-turn retry-bucket state machine. Lives alongside
                // `LoopTurnState` rather than inside it so the file boundary
                // from issue #489 stays exact.
                //
                // Review A F-015: when a persistent retry state is attached
                // via `with_persistent_retry_state`, the guard hydrates from
                // the shared handle on construction and writes back on drop,
                // so bucket counters carry across turns for the same session.
                let mut retry_state =
                    PersistentRetryStateGuard::new(self.persistent_retry_state.clone());
                let mut loop_detector = LoopDetector::new(12);
                // Tools may report that they have already exhausted all
                // meaningful retries for this user turn. Keep that fact in
                // the host, not in the model: a later call with rewritten
                // arguments is still the same terminal operation and must
                // not execute again.
                let mut terminal_tools_for_turn = HashSet::new();
                let mut turn_ledger = self.new_turn_ledger();
                // #1691 (codex gap G6): fire the in-band budget reminder once,
                // when the run first crosses ~80% of its iteration cap.
                let mut budget_reminder_sent = false;

                // UserPromptSubmit lifecycle hook. Fires exactly once here —
                // when a real user-submitted prompt enters the turn, after the
                // messages are assembled but BEFORE the first LLM call and
                // before any loop iteration (per-iteration LLM gating is
                // BeforeLlmCall's job). A before-hook can:
                //   * DENY the prompt (exit 1) → block the whole turn with the
                //     hook's stdout as the surfaced reason (mirrors how a
                //     denied tool call surfaces its reason), or
                //   * INJECT context (exit 0 + stdout) → prepend the hook's
                //     stdout as a per-turn system-context note so it reaches
                //     the model for THIS turn without being persisted as a
                //     message.
                // No-op (byte-identical to before) when no `user_prompt_submit`
                // hook is configured — the executor returns Allow immediately.
                if let Some(ref hooks) = self.hooks {
                    let hook_ctx = self.hook_ctx();
                    let cwd = self.tools.workspace_root().map(|p| p.display().to_string());
                    let payload = HookPayload::user_prompt_submit(
                        user_content,
                        self.llm.model_id(),
                        cwd.as_deref(),
                        hook_ctx.as_ref(),
                    );
                    match hooks.run(HookEvent::UserPromptSubmit, &payload).await {
                        HookResult::Deny(reason) => {
                            let reason = reason.trim();
                            let message = if reason.is_empty() {
                                "[HOOK DENIED] Prompt was blocked by a user_prompt_submit hook."
                                    .to_string()
                            } else {
                                format!(
                                    "[HOOK DENIED] Prompt was blocked by a user_prompt_submit hook: {reason}"
                                )
                            };
                            tracing::warn!(reason = %reason, "user_prompt_submit hook denied prompt");
                            self.reporter().report(ProgressEvent::LlmStatus {
                                message: message.clone(),
                                iteration: 0,
                            });
                            // Mirror the budget-stop early return: persist the
                            // user's message (turn_output_log) and surface the
                            // deny reason as the assistant content. The LLM is
                            // never called.
                            return Ok(ConversationResponse {
                                content: message,
                                reasoning_content: None,
                                provider_metadata: None,
                                token_usage: turn.total_usage().clone(),
                                estimated_spend_usd: turn.priced_spend(),
                                files_modified,
                                files_to_send,
                                streamed: false,
                                messages: turn_output_log.clone(),
                                tool_results: tool_structured_metadata.clone(),
                                synthesized_from_spawn_only: false,
                                pending_approval: None,
                            });
                        }
                        HookResult::Context(contexts) => {
                            // Inject each hook's stdout as a per-turn System
                            // context note, placed right after the primary
                            // system prompt (index 0). The next
                            // `normalize_system_messages` pass folds these into
                            // the system context the model sees. They are NOT
                            // added to `turn_output_log`, so they are never
                            // persisted as conversation messages.
                            for context in contexts.into_iter().rev() {
                                let trimmed = context.trim();
                                if trimmed.is_empty() {
                                    continue;
                                }
                                messages.insert(
                                    1,
                                    Message {
                                        role: MessageRole::System,
                                        content: format!(
                                            "[UserPromptSubmit hook context]\n{trimmed}"
                                        ),
                                        media: vec![],
                                        tool_calls: None,
                                        tool_call_id: None,
                                        reasoning_content: None,
                                        client_message_id: None,
                                        thread_id: None,
                                        timestamp: chrono::Utc::now(),
                                    },
                                );
                            }
                        }
                        HookResult::Allow | HookResult::Error(_) | HookResult::Modified(_) => {}
                    }
                }

                // Labeled so the loop-detector recovery arms below can re-enter
                // the AGENT loop (skip tool execution for this spiraling
                // response), not merely the inner `for tc in &response.tool_calls`
                // loop — an unlabeled `continue` there fell through to
                // `handle_tool_use` and executed the spiraling tools anyway.
                'agent_loop: loop {
                    // Codex-parity steer drain (turn.rs:225-233): fold any
                    // mid-turn injected user inputs into the conversation at
                    // the TOP of each iteration, BEFORE the next LLM call,
                    // in FIFO order. Steering never interrupts an in-flight
                    // round — inputs buffered while the model streams are
                    // picked up here, after the previous round's tool
                    // results are recorded.
                    self.drain_pending_steer_input(&mut messages, &mut turn_output_log)
                        .await;
                    if let Some(stop) = turn.check_budget(self, activity.as_ref()) {
                        let stop_iteration = turn.iteration();
                        if !self.try_budget_grace_call(
                            &stop,
                            &mut retry_state,
                            stop_iteration,
                        ) {
                            turn.record_budget_stop(&stop);
                            // Skip system prompt + history; return only new messages
                            return Ok(ConversationResponse {
                                content: stop.message(),
                                reasoning_content: None,
                                provider_metadata: None,
                                token_usage: turn.total_usage().clone(),
                                estimated_spend_usd: turn.priced_spend(),
                                files_modified,
                                files_to_send,
                                streamed: false,
                                messages: turn_output_log.clone(),
                                tool_results: tool_structured_metadata.clone(),
                                synthesized_from_spawn_only: false,
                                pending_approval: None,
                            });
                        }
                        // #1691: grace was granted (we did not return) — this is
                        // the FINAL iteration. Tell the model to deliver now
                        // rather than start new work, so a grace call is not
                        // wasted on more exploration (the mini4 failure mode).
                        messages.push(Message::user(
                            "[budget notice] This is your FINAL iteration — the run stops \
                             immediately after it. Do NOT start new exploration; write your \
                             deliverable (write_file / edit_file) or give your final answer \
                             in THIS response.",
                        ));
                    }

                    let iteration = turn.advance_iteration();
                    // #1691 (codex gap G6): as the run approaches its iteration
                    // cap, warn the model in-band ONCE (~80%) so a long task
                    // converges on a written deliverable instead of silently
                    // hitting the wall with nothing produced (the mini4
                    // review-worker failure mode). Non-forcing — it only nudges.
                    let max_iters = self.config.max_iterations;
                    if !budget_reminder_sent
                        && max_iters > 0
                        && iteration.saturating_mul(5) >= max_iters.saturating_mul(4)
                    {
                        let remaining = max_iters.saturating_sub(iteration);
                        messages.push(Message::user(format!(
                            "[budget notice] ~{remaining} tool iteration(s) left before this \
                             run is force-stopped at {max_iters}. Stop exploring now and \
                             deliver: write your result with write_file / edit_file (or state \
                             your final answer) before you run out."
                        )));
                        budget_reminder_sent = true;
                    }
                    // Realtime heartbeat: beat first, then abort the iteration
                    // with a typed error if the controller reports stalled.
                    // A None controller / disabled config is a no-op so the
                    // 830+ existing tests see identical behavior.
                    // #1969: a heartbeat interrupt/stall is an error EXIT too —
                    // carry the turn's accumulated usage out with it.
                    if let Err(e) = self.beat_heartbeat(iteration) {
                        return Err(attach_partial_usage(e, turn.total_usage().clone()));
                    }
                    self.reporter()
                        .report(ProgressEvent::Thinking { iteration });

                    // RFC-0 (#1289): LRU tool deferral removed — every enabled
                    // tool is emitted every turn (full schema).
                    let tools_spec = self.tools.specs();
                    // Harness M6.3: run preflight compaction before the first
                    // LLM call when a compaction policy is wired and the
                    // context already exceeds the declared threshold.
                    if iteration == 1 {
                        if let Some(summary) =
                            self.maybe_run_preflight_compaction(&mut messages)?
                        {
                            // #1587 write side: a conversation large enough to
                            // compact on entry is worth recalling later. Upserts
                            // the per-session episode (see save_conversation_episode).
                            self.save_conversation_episode(summary).await;
                        }
                    }
                    // Harness M8.5 tier 1: cheap in-place stale/oversized
                    // tool-result pruning. Runs every iteration (including
                    // the first so large bootstrap payloads shrink before
                    // tier 3 considers whether to summarise).
                    let protected_ids = collect_protected_tool_call_ids(&messages);
                    self.run_tier1_compaction(&mut messages, &protected_ids, tier1_pass(iteration));
                    prepare_conversation_messages(self, &mut messages, &mut turn);
                    // Harness M6.3: post-prep compaction pass so the declarative
                    // runner sees the final shape of the conversation (after
                    // tool-pair repair + system-message normalization). This
                    // also feeds the validator rail on subsequent iterations.
                    if let Some(summary) =
                        self.maybe_run_turn_compaction(&mut messages, iteration)?
                    {
                        // #1587 write side: a conversation that compacts is
                        // substantial enough to be worth recalling later.
                        // Persist the compaction summary as an embedded
                        // episode so a future conversation's session-start
                        // recall can surface it. No-op without an embedder
                        // or when episodes are disabled.
                        self.save_conversation_episode(summary).await;
                    }
                    self.prepare_prompt_with_context_manager(
                        &mut messages,
                        if iteration == 1 {
                            PromptContextPhase::TurnStart
                        } else {
                            PromptContextPhase::Iteration
                        },
                        iteration,
                    );
                    let total_usage = turn.total_usage().clone();

                    if iteration == 1 && tools_spec.len() > 25 {
                        tracing::warn!(
                            tools = tools_spec.len(),
                            "high tool count may cause empty responses with some models; \
                             consider reducing skills (always: false) or adding a tool_policy deny list"
                        );
                    }
                    tracing::info!(
                        iteration,
                        messages = messages.len(),
                        tools = tools_spec.len(),
                        message_bytes = messages.iter().map(|m| m.content.len()).sum::<usize>(),
                        "calling LLM"
                    );
                    // M8.5 tier 2: optionally decorate the outgoing ChatConfig
                    // with the Anthropic `context_management` payload so the
                    // server can clear old tool uses on its side. Non-Anthropic
                    // providers ignore `context_management` via
                    // `skip_serializing_if`.
                    let call_config = with_tier2_context_management(&config, self);
                    let (mut response, streamed, attributed_cost) = match self
                        .call_llm_with_hooks(
                            &messages,
                            &tools_spec,
                            &call_config,
                            iteration,
                            &total_usage,
                            &mut turn,
                        )
                        .await
                    {
                        Ok(r) => r,
                        Err(e) if e.to_string().contains("empty response after") => {
                            // Task 8: under FailFast an empty response is
                            // TERMINAL — do NOT make the adaptive 2nd call.
                            // Emit the voice EmptyResponse projection once and
                            // bail with the original error.
                            if octos_llm::current_llm_call_policy()
                                == octos_llm::LlmCallPolicy::FailFast
                            {
                                if let Some(sink) = &self.voice_failure_sink {
                                    let _ = sink.send(crate::TurnFailure::EmptyResponse);
                                }
                                return Err(attach_partial_usage(e, turn.total_usage().clone()));
                            }
                            // Empty response after retries -- try once more (adaptive router
                            // may select a different provider on this second attempt).
                            turn.record_retry(LoopRetryReason::ProviderFailover {
                                reason: "adaptive failover after empty response".to_string(),
                            });
                            warn!(error = %e, "retrying LLM call for adaptive failover");
                            self.reporter().report(ProgressEvent::LlmStatus {
                                message: "Switching provider...".to_string(),
                                iteration,
                            });
                            match self
                                .call_llm_with_hooks(
                                    &messages,
                                    &tools_spec,
                                    &call_config,
                                    iteration,
                                    &total_usage,
                                    &mut turn,
                                )
                                .await
                            {
                                Ok(r) => r,
                                Err(e) => {
                                    if self.failfast_llm_bail(&e) {
                                        return Err(attach_partial_usage(e, turn.total_usage().clone()));
                                    }
                                    match self.handle_loop_error_with_dispatch(
                                        &e,
                                        &mut retry_state,
                                        iteration,
                                        &mut messages,
                                    ) {
                                        LoopErrorAction::Retry => continue,
                                        LoopErrorAction::Bail => return Err(attach_partial_usage(e, turn.total_usage().clone())),
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            if self.failfast_llm_bail(&e) {
                                return Err(attach_partial_usage(e, turn.total_usage().clone()));
                            }
                            match self.handle_loop_error_with_dispatch(
                                &e,
                                &mut retry_state,
                                iteration,
                                &mut messages,
                            ) {
                                LoopErrorAction::Retry => continue,
                                LoopErrorAction::Bail => return Err(attach_partial_usage(e, turn.total_usage().clone())),
                            }
                        }
                    };
                    Self::normalize_inline_invokes(&mut response);
                    self.reporter().report(ProgressEvent::Response {
                        content: response.content.clone().unwrap_or_default(),
                        iteration,
                    });
                    {
                        let tool_names: Vec<&str> = response
                            .tool_calls
                            .iter()
                            .map(|tc| tc.name.as_str())
                            .collect();
                        let tool_ids: Vec<&str> = response
                            .tool_calls
                            .iter()
                            .map(|tc| tc.id.as_str())
                            .collect();
                        tracing::info!(
                            iteration,
                            stop_reason = ?response.stop_reason,
                            tool_calls = response.tool_calls.len(),
                            tool_names = %tool_names.join(", "),
                            tool_ids = %tool_ids.join(", "),
                            response_content_len = response.content.as_ref().map(|c| c.len()).unwrap_or(0),
                            input_tokens = response.usage.input_tokens,
                            output_tokens = response.usage.output_tokens,
                            // Prompt caching is the largest single cost lever
                            // and is on by default, but it was unobservable
                            // from the logs: these two were already parsed and
                            // handed to `record_usage` below, just never
                            // printed. `input_tokens` alone is misleading —
                            // it EXCLUDES the cached portion, so a warm turn
                            // looks like a tiny prompt rather than a cheap one.
                            cache_read_tokens = response.usage.cache_read_tokens,
                            cache_write_tokens = response.usage.cache_write_tokens,
                            "LLM response received"
                        );
                    }
                    turn.record_usage(
                        response.usage.input_tokens,
                        response.usage.output_tokens,
                        response.usage.cache_read_tokens,
                        response.usage.cache_write_tokens,
                        tracker,
                        // Attributed inside `call_llm_with_hooks`: each
                        // attempt (discarded retries included) priced at the
                        // slot that actually consumed it — re-pricing the
                        // merged usage at the winner's rate here would
                        // misprice cross-provider retries.
                        attributed_cost,
                    );

                    match response.stop_reason {
                        StopReason::EndTurn | StopReason::StopSequence => {
                            let content = response.content.clone().unwrap_or_default();
                            if !self
                                .verifier_allows_termination(
                                    &mut messages,
                                    turn_ledger.as_mut(),
                                    &content,
                                    iteration,
                                    &mut turn,
                                    tracker,
                                )
                                .await?
                            {
                                continue;
                            }
                            // Codex-parity follow-up gate (turn.rs:304-318):
                            // `needs_follow_up = model_needs_follow_up ||
                            // has_pending_input`. The model produced its
                            // final answer, but a steer landed during this
                            // round — record that answer as a normal
                            // assistant row (so the model sees its own
                            // reply next round and the row persists), then
                            // loop again; the top-of-loop drain folds the
                            // steer in as a plain user message and the
                            // steer gets a response inside the SAME turn.
                            if self.steer_input_pending() {
                                if !content.trim().is_empty() {
                                    let mut assistant_row = Message::assistant(content.clone());
                                    assistant_row.reasoning_content =
                                        response.reasoning_content.clone();
                                    messages.push(assistant_row.clone());
                                    turn_output_log.push(assistant_row);
                                }
                                tracing::info!(
                                    "steer input pending at EndTurn — running another round \
                                     instead of terminating the turn"
                                );
                                continue 'agent_loop;
                            }
                            self.emit_cost_update(&turn, &response, attributed_cost);
                            return Ok(ConversationResponse {
                                content,
                                reasoning_content: response.reasoning_content.clone(),
                                provider_metadata: Some(
                                    self.llm.provider_metadata_for_index(response.provider_index),
                                ),
                                token_usage: turn.total_usage().clone(),
                                estimated_spend_usd: turn.priced_spend(),
                                files_modified,
                                files_to_send,
                                streamed,
                                messages: turn_output_log.clone(),
                                tool_results: tool_structured_metadata.clone(),
                                synthesized_from_spawn_only: false,
                                pending_approval: None,
                            });
                        }
                        StopReason::ToolUse => {
                            // Check for loop detection before executing
                            for tc in &response.tool_calls {
                                if terminal_tools_for_turn.contains(&tc.name) {
                                    self.emit_cost_update(&turn, &response, attributed_cost);
                                    warn!(
                                        tool = %tc.name,
                                        "terminal tool retried in the same turn; stopping before execution"
                                    );
                                    return Ok(ConversationResponse {
                                        content: terminal_tool_retry_message(&tc.name),
                                        reasoning_content: None,
                                        provider_metadata: None,
                                        token_usage: turn.total_usage().clone(),
                                        estimated_spend_usd: turn.priced_spend(),
                                        files_modified,
                                        files_to_send,
                                        streamed,
                                        messages: turn_output_log.clone(),
                                        tool_results: tool_structured_metadata.clone(),
                                        synthesized_from_spawn_only: false,
                                        pending_approval: None,
                                    });
                                }
                                // #1765 doom-loop guard: 3+ CONSECUTIVE
                                // identical tool calls (same name + identical
                                // arguments JSON) abort the turn before the
                                // next LLM call. Checked ahead of the cycle
                                // detector so the tighter threshold owns pure
                                // identical streaks; the cycle detector keeps
                                // owning alternating (cycle 2/3) patterns,
                                // which never build a doom streak. When the
                                // guard fires, the shell-spiral recovery is
                                // still consulted first — extracting real
                                // shell output from a retry spiral is a
                                // strictly better outcome than a doom abort.
                                //
                                // Verifier-configured agents are exempt: the
                                // verifier lane classifies each repeated
                                // failure into the turn ledger and injects a
                                // `verdict: Repeating` note the planner acts
                                // on at exactly this streak length — a
                                // strictly richer recovery than an abort
                                // (see `verifier_repeating_note_changes_
                                // next_planner_action`). The cycle detector
                                // below still terminates true thrash there.
                                let doom_streak = if self.verifier_config.is_some() {
                                    None
                                } else {
                                    loop_detector.record_doom(&tc.name, &tc.arguments)
                                };
                                let cycle_warning = if doom_streak.is_some() {
                                    None
                                } else {
                                    loop_detector.record(&tc.name, &tc.arguments)
                                };
                                if doom_streak.is_none() && cycle_warning.is_none() {
                                    continue;
                                }
                                warn!("loop detected — breaking agent loop");
                                {
                                    let spiral_iteration = turn.iteration();
                                    if let Some(outcome) = self
                                        .dispatch_shell_retry_recovery(
                                            &messages,
                                            &mut retry_state,
                                            spiral_iteration,
                                        )
                                    {
                                        // Fix #2 (codex round 2): branch on
                                        // (recovery.kind, decision).
                                        //   - RetryLimit + Escalate: splice
                                        //     the system-shaped instruction
                                        //     into the latest Tool message
                                        //     and continue — the LLM gets ONE
                                        //     iteration to produce a real
                                        //     user-facing summary.
                                        //   - RetryLimit + Exhausted: the
                                        //     model already had its summary
                                        //     chance and ignored it. Don't
                                        //     loop again — return the recovery
                                        //     content as terminal content.
                                        //   - Success kinds: recovery.content
                                        //     is RAW shell output extracted
                                        //     from the noise. Original
                                        //     return-as-content was correct
                                        //     for these.
                                        let should_splice = matches!(
                                            (
                                                &outcome.recovery.kind,
                                                outcome.decision,
                                            ),
                                            (
                                                ShellRetryRecoveryKind::RetryLimit,
                                                LoopDecision::Escalate,
                                            ),
                                        );
                                        if should_splice {
                                            // Codex round-2 #d: target the
                                            // latest SHELL Tool message in
                                            // the trailing batch, not
                                            // whichever Tool happens to be
                                            // last. In a mixed
                                            // `[shell, read_file]` batch
                                            // the trailing Tool is read_file
                                            // — splicing into it would
                                            // mis-attribute the recovery
                                            // instruction and silently drop
                                            // the actual shell output.
                                            if let Some(idx) =
                                                latest_tool_batch_index(&messages, "shell")
                                            {
                                                messages[idx].content = outcome.recovery.content;
                                                warn!(
                                                    "shell spiral fired pre-execution; injected recovery notice into latest shell Tool and continuing for LLM summary"
                                                );
                                                continue 'agent_loop;
                                            }
                                        }
                                        let terminal_content = if matches!(
                                            outcome.recovery.kind,
                                            ShellRetryRecoveryKind::RetryLimit,
                                        ) {
                                            shell_retry_terminal_user_message(
                                                &outcome.recovery.content,
                                            )
                                        } else {
                                            outcome.recovery.content
                                        };
                                        warn!(
                                            recovery_kind = ?outcome.recovery.kind,
                                            decision = %outcome.decision,
                                            "shell spiral terminal: returning recovered content as final assistant reply"
                                        );
                                        self.emit_cost_update(&turn, &response, attributed_cost);
                                        return Ok(ConversationResponse {
                                            content: terminal_content,
                                            reasoning_content: None,
                                            provider_metadata: None,
                                            token_usage: turn.total_usage().clone(),
                                estimated_spend_usd: turn.priced_spend(),
                                            files_modified,
                                            files_to_send,
                                            streamed,
                                            messages: turn_output_log.clone(),
                                            tool_results: tool_structured_metadata.clone(),
                                            synthesized_from_spawn_only: false,
                                pending_approval: None,
                                        });
                                    }
                                    self.emit_cost_update(&turn, &response, attributed_cost);
                                    // #1765 v1 escalation: the doom guard
                                    // aborts the turn with a clear model-and-
                                    // user-facing message — no further LLM
                                    // call, no execution of the tripping
                                    // call. (Interactive continue/edit
                                    // options are follow-up work; v1
                                    // deliberately avoids a new AppUI
                                    // approval kind.)
                                    if let Some(streak) = doom_streak {
                                        self.mark_loop_detected_recently();
                                        warn!(
                                            tool = %tc.name,
                                            streak,
                                            "doom loop detected — aborting turn before the next LLM call (#1765)"
                                        );
                                        return Ok(ConversationResponse {
                                            content: doom_loop_terminal_message(
                                                &tc.name, streak,
                                            ),
                                            reasoning_content: None,
                                            provider_metadata: None,
                                            token_usage: turn.total_usage().clone(),
                                            estimated_spend_usd: turn.priced_spend(),
                                            files_modified,
                                            files_to_send,
                                            streamed,
                                            messages: turn_output_log.clone(),
                                            tool_results: tool_structured_metadata.clone(),
                                            synthesized_from_spawn_only: false,
                                            pending_approval: None,
                                        });
                                    }
                                    let Some(warning) = cycle_warning else {
                                        // Unreachable: doom returned above and
                                        // the earlier gate skipped no-signal
                                        // calls. Kept defensive.
                                        continue;
                                    };
                                    // Two-stage loop-detector recovery:
                                    //
                                    // 1. First fire in this turn — inject the
                                    //    warning as a SYNTHETIC tool-result
                                    //    message paired with the looping
                                    //    assistant message, then continue
                                    //    the loop. The LLM gets one more
                                    //    iteration to synthesise an answer
                                    //    from prior context or switch
                                    //    tools/arguments. This rescues the
                                    //    kimi-k2.5 news_fetch retry spiral
                                    //    documented in PR
                                    //    `fix/news-fetch-loop-and-detect-recovery`
                                    //    (session `web-1779494658716-mxrxe8`,
                                    //    ledger seq 214-562).
                                    //
                                    // 2. Second fire in the same turn — the
                                    //    LLM ignored the warning and looped
                                    //    again. Return a terminal
                                    //    ConversationResponse with a
                                    //    hard-stop message so the user sees
                                    //    a clean reply rather than a thrash.
                                    //
                                    // The single-fire-per-burst flag
                                    // (`loop_detected_recently`) is owned by
                                    // `dedup_loop_warning`. The Err it
                                    // returns on second fire is caught and
                                    // converted to a terminal Ok response
                                    // here so callers don't see an error.
                                    match self.dedup_loop_warning(warning) {
                                        Ok(warning_content) => {
                                            inject_loop_detected_synthetic_results_with_log(
                                                &mut messages,
                                                &response,
                                                &warning_content,
                                                self,
                                                Some(&mut turn_output_log),
                                            );
                                            warn!(
                                                "loop detected — injected synthetic tool results with warning and continuing for ONE more iteration"
                                            );
                                            continue 'agent_loop;
                                        }
                                        Err(_) => {
                                            warn!(
                                                "loop detected AGAIN after warning was already injected — terminating turn"
                                            );
                                            return Ok(ConversationResponse {
                                                content: loop_detected_terminal_message(),
                                                reasoning_content: None,
                                                provider_metadata: None,
                                                token_usage: turn.total_usage().clone(),
                                estimated_spend_usd: turn.priced_spend(),
                                                files_modified,
                                                files_to_send,
                                                streamed,
                                                messages: turn_output_log.clone(),
                                                tool_results: tool_structured_metadata.clone(),
                                                synthesized_from_spawn_only: false,
                                pending_approval: None,
                                            });
                                        }
                                    }
                                }
                            }
                            // Codex round-2 MAJOR 2 (PR #1187 fixup): collect
                            // per-tool-call success bits for THIS iteration
                            // only. Declared fresh inside the loop body so
                            // the spawn_only synth-ack gate reads bits for
                            // the current iteration, never stale bits from
                            // earlier ones in the same turn.
                            let mut iter_tool_success: Vec<(String, bool)> = Vec::new();
                            // Codex round-3 MAJOR (PR #1187 follow-up): bind
                            // the SANITIZED response returned by
                            // `handle_tool_use` so the synth-ack gate below
                            // sees the same tool_call_ids that the
                            // dispatcher keyed `iter_tool_success` by. If we
                            // kept using the original `response`, a real
                            // `success=false` could be missed when
                            // sanitization rewrote the id (colon, empty,
                            // duplicate) — and the content-fallback in the
                            // gate also keys on the original id, so it
                            // misses too. See doc on `handle_tool_use`.
                            let mut iter_pending_approval: Option<
                                crate::approval::PendingApprovalDraft,
                            > = None;
                            let sanitized_response = match self
                                .handle_tool_use(
                                    &response,
                                    &mut messages,
                                    &mut files_modified,
                                    Some(&mut files_to_send),
                                    &mut turn,
                                    &mut retry_state,
                                    tracker,
                                    Some(&mut tool_structured_metadata),
                                    Some(&mut iter_tool_success),
                                    Some(&mut turn_output_log),
                                    &mut loop_detector,
                                    turn_ledger.as_mut(),
                                    Some(&mut terminal_tools_for_turn),
                                    Some(&mut iter_pending_approval),
                                )
                                .await
                            {
                                Ok(sanitized) => sanitized,
                                Err(e) => {
                                    match self.handle_loop_error_with_dispatch(
                                        &e,
                                        &mut retry_state,
                                        iteration,
                                        &mut messages,
                                    ) {
                                        LoopErrorAction::Retry => continue,
                                        LoopErrorAction::Bail => return Err(attach_partial_usage(e, turn.total_usage().clone())),
                                    }
                                }
                            };

                            // Phase 4 (docs/ROBRIX-PHASE4-APPROVAL-FLOW-ADR.md):
                            // a tool call matched a human-approval rule —
                            // suspend the turn. The host (session actor)
                            // projects the request to the channel, stores
                            // the pending approval, and resumes via
                            // `Agent::execute_approved_tool` when an
                            // authorized human answers.
                            if let Some(draft) = iter_pending_approval {
                                self.emit_cost_update(&turn, &sanitized_response, attributed_cost);
                                return Ok(ConversationResponse {
                                    content: String::new(),
                                    reasoning_content: None,
                                    provider_metadata: Some(
                                        self.llm.provider_metadata_for_index(
                                            sanitized_response.provider_index,
                                        ),
                                    ),
                                    token_usage: turn.total_usage().clone(),
                                estimated_spend_usd: turn.priced_spend(),
                                    files_modified,
                                    files_to_send,
                                    streamed,
                                    messages: turn_output_log.clone(),
                                    tool_results: tool_structured_metadata.clone(),
                                    synthesized_from_spawn_only: false,
                                    pending_approval: Some(draft),
                                });
                            }

                            if let Err(e) = self
                                .maybe_run_verifier_after_tool_batch(
                                    &mut messages,
                                    turn_ledger.as_mut(),
                                    iteration,
                                    &mut turn,
                                    tracker,
                                )
                                .await
                            {
                                match self.handle_loop_error_with_dispatch(
                                    &e,
                                    &mut retry_state,
                                    iteration,
                                    &mut messages,
                                ) {
                                    LoopErrorAction::Retry => continue,
                                    LoopErrorAction::Bail => return Err(attach_partial_usage(e, turn.total_usage().clone())),
                                }
                            }

                            let spiral_iteration = turn.iteration();
                            if let Some(outcome) = self.dispatch_shell_retry_recovery(
                                &messages,
                                &mut retry_state,
                                spiral_iteration,
                            ) {
                                // Fix #2 (codex round 2): see
                                // ShellSpiralOutcome doc — only splice +
                                // continue on (RetryLimit, Escalate).
                                // Everything else (RetryLimit+Exhausted,
                                // success-kind extractions) returns the
                                // recovery content as the terminal assistant
                                // reply, matching original behaviour for the
                                // success kinds and bounding the LLM-summary
                                // attempt to a single shot for RetryLimit.
                                let should_splice = matches!(
                                    (&outcome.recovery.kind, outcome.decision),
                                    (
                                        ShellRetryRecoveryKind::RetryLimit,
                                        LoopDecision::Escalate,
                                    ),
                                );
                                if should_splice {
                                    // Codex round-2 #d: target latest SHELL
                                    // Tool, not last Tool. See pre-execution
                                    // call site for rationale.
                                    if let Some(idx) =
                                        latest_tool_batch_index(&messages, "shell")
                                    {
                                        messages[idx].content = outcome.recovery.content;
                                        warn!(
                                            "shell spiral fired post-execution; injected recovery notice into latest shell Tool and continuing for LLM summary"
                                        );
                                        continue;
                                    }
                                }
                                let terminal_content = if matches!(
                                    outcome.recovery.kind,
                                    ShellRetryRecoveryKind::RetryLimit,
                                ) {
                                    shell_retry_terminal_user_message(&outcome.recovery.content)
                                } else {
                                    outcome.recovery.content
                                };
                                warn!(
                                    recovery_kind = ?outcome.recovery.kind,
                                    decision = %outcome.decision,
                                    "shell spiral terminal: returning recovered content as final assistant reply"
                                );
                                self.emit_cost_update(&turn, &response, attributed_cost);
                                return Ok(ConversationResponse {
                                    content: terminal_content,
                                    reasoning_content: None,
                                    provider_metadata: Some(
                                        self.llm.provider_metadata_for_index(
                                            response.provider_index,
                                        ),
                                    ),
                                    token_usage: turn.total_usage().clone(),
                                estimated_spend_usd: turn.priced_spend(),
                                    files_modified,
                                    files_to_send,
                                    streamed,
                                    messages: turn_output_log.clone(),
                                    tool_results: tool_structured_metadata.clone(),
                                    synthesized_from_spawn_only: false,
                                pending_approval: None,
                                });
                            }

                            // Codex round-2 MAJOR 1 (PR #1187 fixup):
                            // the previous gate read
                            // `self.tools.spawn_only_was_invoked()`, which is
                            // a TURN-wide AtomicBool set by `execution.rs`
                            // when ANY iteration in the turn invokes a
                            // spawn_only tool. Once flipped it stays true
                            // until the next turn begins, so on a
                            // multi-iteration turn the LLM could call
                            // run_pipeline (spawn_only) in iter 1, get an
                            // error response, react by calling read_file
                            // (regular) in iter 2, then EndTurn in iter 3 —
                            // and the iter-2 ToolUse arm would still see
                            // the flag set and synthesise "Background work
                            // started." even though THIS iteration never
                            // touched a spawn_only tool. The synth-ack is
                            // only ever appropriate when the CURRENT
                            // iteration's response actually contains a
                            // spawn_only tool call, so gate on that
                            // directly.
                            let current_iter_has_spawn_only = response
                                .tool_calls
                                .iter()
                                .any(|tc| self.tools.is_spawn_only(&tc.name));
                            if current_iter_has_spawn_only {
                                // Fleet-UX soak B4 (mini1 / dspfac, 2026-05-22):
                                // when the LLM called a spawn_only tool AND
                                // any tool in the same turn-batch produced an
                                // error-shaped result (pre-flight rejection,
                                // provider/hook deny, panic, timeout, or
                                // sibling-cancel in a serial batch), the
                                // synthesized "Background work started for
                                // `<tool>`." acknowledgement would sit
                                // alongside the red error chip the UI
                                // already renders for the failed tool — a
                                // confusing dual signal where the user sees
                                // both a successful-looking ack bubble and a
                                // failed-tool chip for the same turn.
                                //
                                // When the gate fires, skip the synthesized
                                // ack and fall through to the normal
                                // next-iteration path so the LLM sees the
                                // error tool result and can react. The
                                // background task — when one was actually
                                // dispatched — still completes asynchronously
                                // and routes its outcome via the
                                // BackgroundResultSender, so the legitimate
                                // "task finished" signal still arrives on
                                // that channel; we only suppress the
                                // turn-final fabricated "started" bubble
                                // that the foreground can't actually verify.
                                // Codex round-3 MAJOR (PR #1187 follow-up):
                                // pass the SANITIZED response so the
                                // tool_call_id keys here line up with the
                                // ones the dispatcher used for
                                // `iter_tool_success`. Using the original
                                // `response` here is the bug: sanitization
                                // may have rewritten an id (colon, empty,
                                // duplicate) and the lookup would miss,
                                // letting a real `success=false` slip past
                                // the gate.
                                if any_tool_invocation_errored(
                                    &messages,
                                    &sanitized_response,
                                    &iter_tool_success,
                                ) {
                                    warn!(
                                        "tool invocation errored in spawn_only turn — suppressing synthesized 'Background work started' ack and letting the LLM react to the error"
                                    );
                                } else {
                                    let should_gate_spawn_ack = turn_ledger
                                        .as_ref()
                                        .is_some_and(TurnLedger::ready_gate_active);
                                    if should_gate_spawn_ack
                                        && !self
                                            .verifier_allows_termination(
                                                &mut messages,
                                                turn_ledger.as_mut(),
                                                "Background work started.",
                                                iteration,
                                                &mut turn,
                                                tracker,
                                            )
                                            .await?
                                    {
                                        continue;
                                    }
                                    self.emit_cost_update(&turn, &response, attributed_cost);
                                    // Post-spawn failure feedback loop
                                    // (feat/spawn-only-failure-feedback-loop):
                                    // record that the synth-ack went out for
                                    // every spawn_only tool_call_id in this
                                    // turn. The supervisor's `notify_failure`
                                    // gates `SpawnOnlyFailureSignal` emission
                                    // on this set so an eventual post-spawn
                                    // failure (Gemini API error, plugin
                                    // crash, late validator rejection) can
                                    // reach the session actor and drive a
                                    // recovery turn. Sibling-error
                                    // suppression (the `if` branch above)
                                    // intentionally skips this — the LLM
                                    // already saw the sibling's error
                                    // tool_result.
                                    //
                                    // Codex round-4 MAJOR (PR #1324 follow-up):
                                    // iterate `sanitized_response.tool_calls`
                                    // — not `response.tool_calls` — so the
                                    // recorded id matches the one the
                                    // dispatcher used to register the
                                    // background task in
                                    // `execution.rs::register_task_with_input_and_cmid`.
                                    // `handle_tool_use` rewrites every
                                    // tool_call_id via `sanitize_tool_call_id`
                                    // (colon → underscore character
                                    // sanitisation), and the supervisor
                                    // stores the sanitized id on the
                                    // `BackgroundTask`. Codex round
                                    // (PR #1355) removed the
                                    // empty/duplicate id repair from this
                                    // path; ids should arrive correct from
                                    // the provider.
                                    // Recording the ORIGINAL `tc.id` here
                                    // (e.g. `call:1`) would key the
                                    // synth-ack set on a value that
                                    // `notify_failure` never looks up
                                    // (it checks the sanitized `call_1`),
                                    // permanently dropping the recovery
                                    // signal. The `background_tools` chip
                                    // collection uses the sanitized response
                                    // for the same reason — it stays in
                                    // lock-step with what the LLM observed.
                                    let supervisor = self.tools.supervisor();
                                    for tc in &sanitized_response.tool_calls {
                                        if self.tools.is_spawn_only(&tc.name) {
                                            supervisor.mark_synth_ack_emitted(&tc.id);
                                        }
                                    }
                                    let background_tools = sanitized_response
                                        .tool_calls
                                        .iter()
                                        .filter(|tc| self.tools.is_spawn_only(&tc.name))
                                        .map(|tc| tc.name.as_str())
                                        .collect::<Vec<_>>();
                                    let content = if background_tools.is_empty() {
                                        "Background work started. The final result will be delivered automatically when it is ready.".to_string()
                                    } else if background_tools.len() == 1 {
                                        format!(
                                            "Background work started for `{}`. The final result will be delivered automatically when it is ready.",
                                            background_tools[0]
                                        )
                                    } else {
                                        format!(
                                            "Background work started for {} tasks ({}). The final results will be delivered automatically when they are ready.",
                                            background_tools.len(),
                                            background_tools.join(", ")
                                        )
                                    };
                                    return Ok(ConversationResponse {
                                        content,
                                        reasoning_content: None,
                                        provider_metadata: Some(
                                            self.llm.provider_metadata_for_index(response.provider_index),
                                        ),
                                        token_usage: turn.total_usage().clone(),
                                estimated_spend_usd: turn.priced_spend(),
                                        files_modified,
                                        files_to_send,
                                        streamed,
                                        messages: turn_output_log.clone(),
                                        tool_results: tool_structured_metadata.clone(),
                                        // dspfac "two bubbles per turn" fix: this
                                        // branch synthesises `content` as the
                                        // "Background work started for `<tool>`..."
                                        // acknowledgement. The API persist site
                                        // reads this flag and skips that
                                        // synthesized row entirely; the actual
                                        // background result later emits its
                                        // canonical v2 child envelope.
                                        synthesized_from_spawn_only: true,
                                pending_approval: None,
                                    });
                                }
                            }
                        }
                        StopReason::MaxTokens => {
                            self.emit_cost_update(&turn, &response, attributed_cost);
                            return Ok(ConversationResponse {
                                content: response.content.unwrap_or_default(),
                                reasoning_content: response.reasoning_content.clone(),
                                provider_metadata: Some(
                                    self.llm.provider_metadata_for_index(response.provider_index),
                                ),
                                token_usage: turn.total_usage().clone(),
                                estimated_spend_usd: turn.priced_spend(),
                                files_modified,
                                files_to_send,
                                streamed,
                                messages: turn_output_log.clone(),
                                tool_results: tool_structured_metadata.clone(),
                                synthesized_from_spawn_only: false,
                                pending_approval: None,
                            });
                        }
                        StopReason::ContentFiltered => {
                            // After retries in call_llm_with_hooks, content is still filtered.
                            // Return a user-visible message instead of empty content.
                            self.emit_cost_update(&turn, &response, attributed_cost);
                            warn!("content filtered by provider safety/moderation after retries");
                            return Ok(ConversationResponse {
                                content: response.content.unwrap_or_else(|| {
                                    "[Content was blocked by the model's safety filter. \
                                     Please rephrase your request.]"
                                        .to_string()
                                }),
                                reasoning_content: None,
                                provider_metadata: Some(
                                    self.llm.provider_metadata_for_index(response.provider_index),
                                ),
                                token_usage: turn.total_usage().clone(),
                                estimated_spend_usd: turn.priced_spend(),
                                files_modified,
                                files_to_send,
                                streamed,
                                messages: turn_output_log.clone(),
                                tool_results: tool_structured_metadata.clone(),
                                synthesized_from_spawn_only: false,
                                pending_approval: None,
                            });
                        }
                    }
                }
                }),
            )
            .await
    }
    /// Run a task to completion (used by spawn tool).
    pub async fn run_task(&self, task: &Task) -> Result<TaskResult> {
        self.run_task_inner(task, None).await
    }

    /// Like [`Agent::run_task`], but stores the turn-cumulative token counts
    /// into `tracker` after every LLM response. A caller that runs the task
    /// under an external wall-clock timeout — which DROPS the future and
    /// discards the returned [`TaskResult`] — can then still read the REAL
    /// tokens the run spent. The fleet worker uses this to settle a mid-task
    /// escalation's budget honestly even on the timeout / run-error path (never
    /// `0`), mirroring the conversation loop's real-time tracker.
    pub async fn run_task_with_tracker(
        &self,
        task: &Task,
        tracker: &TokenTracker,
    ) -> Result<TaskResult> {
        self.run_task_inner(task, Some(tracker)).await
    }

    async fn run_task_inner(
        &self,
        task: &Task,
        tracker: Option<&TokenTracker>,
    ) -> Result<TaskResult> {
        let task_start = Instant::now();
        let span = info_span!(
            "task",
            task_id = %task.id,
            agent_id = %self.id,
        );

        let activity = Arc::new(LoopActivityState::new(task_start));
        let activity_reporter = Arc::new(ActivityTrackingReporter::new(
            activity.clone(),
            self.reporter(),
        ));

        TASK_REPORTER
            .scope(activity_reporter, async move {
            info!("starting task");
            self.reporter().report(ProgressEvent::TaskStarted {
                task_id: task.id.to_string(),
            });

            let mut messages = self.build_initial_messages(task).await;
            let mut files_modified = Vec::new();
            let mut files_to_send = Vec::new();
            let mut turn = LoopTurnState::new(task_start);
            let mut max_token_continuations = 0usize;
            let mut max_token_fragments = Vec::new();
            // M6.2: per-run retry-bucket state machine. Same instance lives
            // across all iterations of the task loop so bucket counters
            // accumulate the way operators expect.
            //
            // Review A F-015: hydrate from the persistent handle when set so
            // task buckets survive across repeated `run_task` invocations on
            // the same session (the guard's `Drop` impl writes back).
            let mut retry_state =
                PersistentRetryStateGuard::new(self.persistent_retry_state.clone());
            // PR #1363: task-loop gets its own detector so handle_tool_use
            // can run the no-progress soft check on tool results here too.
            // Matches the conversation-loop's window of 12.
            let mut loop_detector = LoopDetector::new(12);
            let mut turn_ledger = self.new_turn_ledger();
            let config = self.chat_config();

            loop {
                if let Some(stop) = turn.check_budget(self, activity.as_ref()) {
                    let stop_iteration = turn.iteration();
                    if !self.try_budget_grace_call(
                        &stop,
                        &mut retry_state,
                        stop_iteration,
                    ) {
                        turn.record_budget_stop(&stop);
                        self.report_budget_stop(&stop, stop_iteration);
                        return Ok(TaskResult {
                            schema_version: octos_core::TASK_RESULT_SCHEMA_VERSION,
                            success: false,
                            output: stop.message(),
                            files_modified,
                            files_to_send,
                            subtasks: Vec::new(),
                            token_usage: turn.total_usage().clone(),
                        });
                    }
                }

                let iteration = turn.advance_iteration();
                let iter_start = Instant::now();
                // Realtime heartbeat beat + stall check (no-op when realtime
                // is disabled or unattached).
                // #1969: carry accumulated usage out on an interrupt/stall exit.
                if let Err(e) = self.beat_heartbeat(iteration) {
                    return Err(attach_partial_usage(e, turn.total_usage().clone()));
                }
                self.reporter()
                    .report(ProgressEvent::Thinking { iteration });

                // RFC-0 (#1289): LRU tool deferral removed — every enabled
                // tool is emitted every turn (full schema).
                let tools_spec = self.tools.specs();
                // M8.5 tier 1: also runs in task mode so background workers
                // benefit from the same cheap shrinkage before their LLM call.
                let protected_ids = collect_protected_tool_call_ids(&messages);
                self.run_tier1_compaction(&mut messages, &protected_ids, tier1_pass(iteration));
                prepare_task_messages(self, &mut messages, &mut turn);
                self.prepare_prompt_with_context_manager(
                    &mut messages,
                    if iteration == 1 {
                        PromptContextPhase::TurnStart
                    } else {
                        PromptContextPhase::Iteration
                    },
                    iteration,
                );
                let total_usage = turn.total_usage().clone();

                // M8.5 tier 2: decorate the config with the Anthropic header.
                let call_config = with_tier2_context_management(&config, self);
                let (mut response, _streamed, attributed_cost) = match self
                    .call_llm_with_hooks(
                        &messages,
                        &tools_spec,
                        &call_config,
                        iteration,
                        &total_usage,
                        &mut turn,
                    )
                    .await
                {
                    Ok(pair) => pair,
                    Err(e) => {
                        if self.failfast_llm_bail(&e) {
                            return Err(attach_partial_usage(e, turn.total_usage().clone()));
                        }
                        match self.handle_loop_error_with_dispatch(
                            &e,
                            &mut retry_state,
                            iteration,
                            &mut messages,
                        ) {
                            LoopErrorAction::Retry => continue,
                            LoopErrorAction::Bail => return Err(attach_partial_usage(e, turn.total_usage().clone())),
                        }
                    }
                };
                Self::normalize_inline_invokes(&mut response);
                turn.record_usage(
                    response.usage.input_tokens,
                    response.usage.output_tokens,
                    response.usage.cache_read_tokens,
                    response.usage.cache_write_tokens,
                    tracker,
                    // Attributed per attempt inside `call_llm_with_hooks`
                    // (cross-provider retries priced at their own slot).
                    attributed_cost,
                );

                let tool_names: Vec<&str> = response
                    .tool_calls
                    .iter()
                    .map(|tc| tc.name.as_str())
                    .collect();
                info!(
                    iteration,
                    input_tokens = response.usage.input_tokens,
                    output_tokens = response.usage.output_tokens,
                    stop_reason = ?response.stop_reason,
                    tool_calls = response.tool_calls.len(),
                    tool_names = %tool_names.join(","),
                    response_content_len = response.content.as_deref().map(|s| s.len()).unwrap_or(0),
                    duration_ms = iter_start.elapsed().as_millis() as u64,
                    "task LLM response"
                );
                // Mirror the conversation loop (`process_message_inner`):
                // surface the assistant's text to the reporter. The task loop
                // never emitted `Response`, so a task-run agent's reporter —
                // e.g. the spawn child's transcript reporter streaming into
                // the SubAgentOutputRouter — saw tool events but none of the
                // model's own words (mini4 re-review: the agent view showed
                // status with "nothing coming out").
                if let Some(content) = response.content.as_deref() {
                    if !content.trim().is_empty() {
                        self.reporter().report(ProgressEvent::Response {
                            content: content.to_string(),
                            iteration,
                        });
                    }
                }

                match response.stop_reason {
                    StopReason::EndTurn | StopReason::StopSequence => {
                        let final_response =
                            response_with_max_token_fragments(&response, &max_token_fragments);
                        let proposed = final_response.content.clone().unwrap_or_default();
                        if !self
                            .verifier_allows_termination(
                                &mut messages,
                                turn_ledger.as_mut(),
                                &proposed,
                                iteration,
                                &mut turn,
                                None,
                            )
                            .await?
                        {
                            continue;
                        }
                        if self.config.save_episodes {
                            let summary = final_response.content.clone().unwrap_or_default();
                            let summary_truncated =
                                octos_core::truncated_utf8(&summary, 500, "...");

                            let mut episode = Episode::new(
                                task.id.clone(),
                                self.id.clone(),
                                task.context.working_dir.clone(),
                                summary_truncated.clone(),
                                EpisodeOutcome::Success,
                            );
                            episode.files_modified = files_modified.clone();
                            let ep_id = episode.id.clone();

                            if let Err(e) = self.memory.store(episode).await {
                                warn!(error = %e, "failed to save episode to memory");
                            }

                            // Fire-and-forget: embed summary and store embedding
                            if let Some(ref embedder) = self.embedder {
                                let embedder = embedder.clone();
                                let memory = self.memory.clone();
                                let summary_text = summary_truncated;
                                let episode_id = ep_id;
                                tokio::spawn(async move {
                                    match embedder.embed(&[&summary_text]).await {
                                        Ok(vecs) => {
                                            if let Some(vec) = vecs.into_iter().next() {
                                                if let Err(e) =
                                                    memory.store_embedding(&episode_id, vec).await
                                                {
                                                    warn!(error = %e, "failed to store embedding");
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            warn!(
                                                error = %e,
                                                episode_id = %episode_id,
                                                "failed to generate embedding for episode"
                                            );
                                        }
                                    }
                                });
                            }
                        }

                        self.emit_cost_update(&turn, &final_response, attributed_cost);

                        // Audit Gap-8: auto-fire `check_workspace_contract`
                        // on Completion. The LLM-callable wrapper stays for
                        // introspection but no longer the only enforcement
                        // path — the harness consults the contract before
                        // declaring SUCCESS.
                        //
                        // Workspaces without a policy-managed repo under the
                        // working_dir stay Success unchanged (returns
                        // `None`). When at least one policy-managed repo is
                        // not ready, the result is demoted to `success =
                        // false` and the failing validators are appended to
                        // the result output so the caller (or LLM next turn)
                        // sees the contract failure.
                        //
                        // octos #997 (round-2 fix): RUN declared project-root
                        // validators BEFORE inspecting the contract. The
                        // contract gate reads
                        // `<kind>/<slug>/.octos/validator_outcomes.jsonl` — a
                        // path that was never written to in production
                        // pre-round-2 because the declared validator chain
                        // was only invoked at the SESSION root. Without this
                        // call, a real valid deck whose project policy
                        // declares a hard-required validator (octos #997:
                        // `slides.mofa_slides.pptx_magic_bytes`) shows
                        // `ready = false` purely because the persisted
                        // outcome is missing.
                        let _project_root_report =
                            crate::workspace_contract::run_project_root_validators(
                                self.tools.as_ref(),
                                &task.context.working_dir,
                                None,
                                &files_to_send,
                                // #1607: the Agent's own registry is built
                                // sandboxed in `session_actor`, so its stored
                                // sandbox is the session backend.
                                self.tools.sandbox(),
                            )
                            .await;
                        let contract_failures =
                            inspect_workspace_contract_failures(&task.context.working_dir);

                        self.reporter().report(ProgressEvent::TaskCompleted {
                            success: contract_failures.is_none(),
                            iterations: iteration,
                            duration: task_start.elapsed(),
                        });

                        info!(
                            total_input_tokens = turn.total_usage().input_tokens,
                            total_output_tokens = turn.total_usage().output_tokens,
                            iterations = iteration,
                            files_modified = files_modified.len(),
                            duration_ms = task_start.elapsed().as_millis() as u64,
                            contract_failed = contract_failures.is_some(),
                            "task completed"
                        );
                        let mut result = self.build_result(
                            &final_response,
                            turn.total_usage().clone(),
                            files_modified,
                            files_to_send,
                        );
                        if let Some(failure_msg) = contract_failures {
                            warn!(
                                workspace_root = %task.context.working_dir.display(),
                                "task EndTurn but workspace contract is not ready; demoting to ContractFailed"
                            );
                            result.success = false;
                            if result.output.is_empty() {
                                result.output = failure_msg;
                            } else {
                                result.output = format!("{}\n\n{}", result.output, failure_msg);
                            }
                        }
                        return Ok(result);
                    }
                    StopReason::ToolUse => {
                        // Task loop never emits the synth-ack so the per-call
                        // success-bit sink is unused here — pass `None`. (The
                        // conversation loop wires this up to the spawn_only
                        // gate; see the matching call site above.) Codex
                        // round-3: ignore the sanitized response too — task
                        // loop has no synth-ack gate that would need it.
                        if let Err(e) = self
                            .handle_tool_use(
                                &response,
                                &mut messages,
                                &mut files_modified,
                                Some(&mut files_to_send),
                                &mut turn,
                                &mut retry_state,
                                None,
                                None,
                                None,
                                None,
                                &mut loop_detector,
                                turn_ledger.as_mut(),
                                None,
                                // Background task loop: no resume host —
                                // rule-matched tools are denied, not
                                // suspended (see handle_tool_use doc).
                                None,
                            )
                            .await
                        {
                            match self.handle_loop_error_with_dispatch(
                                &e,
                                &mut retry_state,
                                iteration,
                                &mut messages,
                            ) {
                                LoopErrorAction::Retry => continue,
                                LoopErrorAction::Bail => return Err(attach_partial_usage(e, turn.total_usage().clone())),
                            }
                        }
                        if let Err(e) = self
                            .maybe_run_verifier_after_tool_batch(
                                &mut messages,
                                turn_ledger.as_mut(),
                                iteration,
                                &mut turn,
                                None,
                            )
                            .await
                        {
                            match self.handle_loop_error_with_dispatch(
                                &e,
                                &mut retry_state,
                                iteration,
                                &mut messages,
                            ) {
                                LoopErrorAction::Retry => continue,
                                LoopErrorAction::Bail => return Err(attach_partial_usage(e, turn.total_usage().clone())),
                            }
                        }
                    }
                    StopReason::MaxTokens => {
                        if max_token_continuations < MAX_TOKENS_CONTINUATION_LIMIT {
                            if let Some(content) = response.content.clone() {
                                if !content.trim().is_empty() {
                                    max_token_fragments.push(content);
                                }
                            }
                            push_max_tokens_continuation(&mut messages, &response);
                            max_token_continuations += 1;
                            warn!(
                                iteration,
                                continuation = max_token_continuations,
                                max = MAX_TOKENS_CONTINUATION_LIMIT,
                                "task output hit max_tokens; continuing in the same agent loop"
                            );
                            continue;
                        }

                        let final_response =
                            response_with_max_token_fragments(&response, &max_token_fragments);
                        self.emit_cost_update(&turn, &final_response, attributed_cost);
                        self.reporter().report(ProgressEvent::TaskCompleted {
                            success: false,
                            iterations: iteration,
                            duration: task_start.elapsed(),
                        });
                        return Ok(self.build_result(
                            &final_response,
                            turn.total_usage().clone(),
                            files_modified,
                            files_to_send,
                        ));
                    }
                    StopReason::ContentFiltered => {
                        warn!("content filtered by provider safety/moderation in task");
                        self.emit_cost_update(&turn, &response, attributed_cost);
                        self.reporter().report(ProgressEvent::TaskCompleted {
                            success: false,
                            iterations: iteration,
                            duration: task_start.elapsed(),
                        });
                        let mut result = self.build_result(
                            &response,
                            turn.total_usage().clone(),
                            files_modified,
                            files_to_send,
                        );
                        if result.output.is_empty() {
                            result.output =
                                "[Content was blocked by the model's safety filter.]".to_string();
                        }
                        return Ok(result);
                    }
                }
            }
            })
            .instrument(span)
            .await
    }

    fn build_result(
        &self,
        response: &ChatResponse,
        usage: TokenUsage,
        files_modified: Vec<std::path::PathBuf>,
        files_to_send: Vec<std::path::PathBuf>,
    ) -> TaskResult {
        let truncated = response.stop_reason == StopReason::MaxTokens;
        let success = !truncated;
        let mut output = response.content.clone().unwrap_or_default();
        if truncated {
            let marker = "[partial output: max_output_tokens reached before a final answer]";
            output = if output.trim().is_empty() {
                marker.to_string()
            } else {
                format!("{marker}\n\n{output}")
            };
        }
        TaskResult {
            schema_version: octos_core::TASK_RESULT_SCHEMA_VERSION,
            success,
            output,
            files_modified,
            files_to_send,
            subtasks: Vec::new(),
            token_usage: octos_core::TokenUsage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cache_read_tokens: usage.cache_read_tokens,
                cache_write_tokens: usage.cache_write_tokens,
                ..Default::default()
            },
        }
    }

    /// Execute tool calls from an LLM response and accumulate results.
    ///
    /// On success returns the SANITIZED response — IDs after
    /// `sanitize_tool_call_id` (character normalisation) + name+args dedup.
    /// Callers that subsequently key into `tool_success_by_id` MUST use the
    /// sanitized response so the lookup matches; the original response's
    /// tool_call_ids are stale once sanitization rewrites them.
    ///
    /// Codex round (PR #1355): the prior "empty/duplicate tool_call_id"
    /// repair step that lived alongside `sanitize_tool_call_id` was removed.
    /// Tool call ids should arrive correct from the provider; if they don't,
    /// that's an upstream provider-impl bug to fix at the source, not by
    /// salvaging downstream.
    ///
    /// Codex round-3 MAJOR (PR #1187 follow-up): the prior signature returned
    /// `Result<()>`, leaving the synth-ack gate at the call site to feed the
    /// CALLER'S original `response` into `any_tool_invocation_errored`. When
    /// sanitization changed an ID (colon, empty, duplicate) the success-bit
    /// lookup in the gate missed and the content-fallback also missed (it
    /// keys on the original ID too), so a real `success=false` slipped past
    /// and synth-ack still fired alongside the red error chip.
    #[allow(clippy::too_many_arguments)]
    async fn handle_tool_use(
        &self,
        response: &ChatResponse,
        messages: &mut Vec<Message>,
        files_modified: &mut Vec<PathBuf>,
        files_to_send: Option<&mut Vec<PathBuf>>,
        turn: &mut LoopTurnState,
        retry_state: &mut LoopRetryState,
        tracker: Option<&TokenTracker>,
        tool_structured_metadata: Option<&mut Vec<(String, serde_json::Value)>>,
        // Codex round-2 MAJOR 2 (PR #1187 fixup): out-parameter that, when
        // supplied, receives the per-tool-call success bit keyed by
        // `tool_call_id`. The conversation-loop call site uses this to
        // gate the synth-ack branch authoritatively (rather than reading
        // the content shape of each tool message). Background callers
        // pass `None` because the task-loop never emits the synth-ack.
        tool_success_by_id: Option<&mut Vec<(String, bool)>>,
        // NEW-16: append-only per-turn output log sink for the
        // conversation loop. When supplied, the SAME assistant message
        // and merged tool-result rows that go into `messages` are also
        // appended here. The task loop passes `None` (it returns
        // `TaskResult`, not `ConversationResponse`, so no log is
        // needed there).
        turn_output_log: Option<&mut Vec<Message>>,
        // PR #1363 (this PR): the outer-scope LoopDetector. Hard cycle
        // detection runs in the caller BEFORE this is invoked (so the
        // turn can be terminated cleanly); the soft "no progress" hint
        // — which augments the just-produced tool result — runs INSIDE
        // this function because we need the actual result content. The
        // soft check is non-terminating: if it fires, the hint is
        // appended to the matching Tool message's content and the
        // conversation continues.
        loop_detector: &mut LoopDetector,
        turn_ledger: Option<&mut TurnLedger>,
        // A failed tool can set structured_metadata.do_not_retry_same_turn.
        // The conversation loop stores the tool name here and rejects any
        // later call before execution, even when the model changes arguments.
        // Background task loops pass None because they have no user turn.
        terminal_tools_for_turn: Option<&mut HashSet<String>>,
        // Phase 4 (docs/ROBRIX-PHASE4-APPROVAL-FLOW-ADR.md): out-parameter
        // for the suspend-and-resume human-approval flow. When a tool call
        // matches a configured `human_approval_rules` rule:
        // - `Some(slot)` (conversation loop): the slot receives the pending
        //   draft, placeholder tool-results are recorded, and NO tool in the
        //   batch executes — the caller suspends the turn so the host can
        //   project the request to the channel and resume later.
        // - `None` (background/task loop): there is no resume host, so the
        //   matched call is denied with an "approval not available" tool
        //   result instead of being silently bypassed.
        pending_approval_out: Option<&mut Option<crate::approval::PendingApprovalDraft>>,
    ) -> Result<ChatResponse> {
        // Sanitize tool_call_id characters: some providers (e.g. Moonshot/kimi)
        // generate IDs like "admin_view_sessions:11" which OpenAI rejects (only
        // letters, numbers, underscores, dashes accepted). This is a documented
        // cross-provider portability concern, not a streaming-layer salvage.
        //
        // Codex round (PR #1355): the "fixing empty/duplicate tool_call_id"
        // salvager that previously lived here (mint a synthetic id when the
        // provider returned an empty / duplicate one) was deleted. Tool call
        // IDs should arrive correct from the provider; if they don't, that's
        // an upstream provider-impl bug to fix at the source, not by salvaging
        // downstream. See `docs/STREAMING-TRANSACTIONAL-BOUNDARY-ADR.md`.
        let mut response = response.clone();
        for tc in response.tool_calls.iter_mut() {
            tc.id = sanitize_tool_call_id(&tc.id);
        }

        // Deduplicate tool calls with identical name + arguments (some models
        // return the same call twice, wasting execution).
        {
            let orig_len = response.tool_calls.len();
            let mut seen_calls = std::collections::HashSet::new();
            response.tool_calls.retain(|tc| {
                let key = format!("{}:{}", tc.name, tc.arguments);
                seen_calls.insert(key)
            });
            if response.tool_calls.len() < orig_len {
                tracing::warn!(
                    removed = orig_len - response.tool_calls.len(),
                    "removed duplicate tool calls (same name+arguments)"
                );
            }
        }
        let assistant_msg = self.response_to_message(&response);
        messages.push(assistant_msg.clone());

        // Phase 4 (docs/ROBRIX-PHASE4-APPROVAL-FLOW-ADR.md): human-approval
        // interception. When any callable tool call in this batch matches a
        // configured rule, no tool in the batch executes — every call gets a
        // placeholder tool-result so the LLM history stays consistent, and
        // the turn suspends with the first matching call's approval draft.
        // This intentionally deviates from the PR #345 reference (which
        // executed non-matching peers): not executing anything around a
        // gated call is simpler and strictly safer.
        if let Some(rules) = self.config.human_approval_rules.as_ref() {
            let matched = response
                .tool_calls
                .iter()
                .find(|tc| {
                    self.tools.get(&tc.name).is_some() && rules.matching_rule(&tc.name).is_some()
                })
                .map(|tc| (tc.name.clone(), tc.id.clone(), tc.arguments.clone()));
            if let Some((matched_name, matched_id, matched_args)) = matched {
                let has_resume_host = pending_approval_out.is_some();
                let outcome = rules.draft_for_tool_call(
                    &matched_name,
                    &matched_id,
                    matched_args,
                    chrono::Utc::now(),
                );
                let tool_placeholder = |tc_id: &str, content: String| Message {
                    role: MessageRole::Tool,
                    content,
                    media: vec![],
                    tool_calls: None,
                    tool_call_id: Some(tc_id.to_string()),
                    reasoning_content: None,
                    client_message_id: None,
                    thread_id: None,
                    timestamp: chrono::Utc::now(),
                };
                match outcome {
                    Ok(Some(draft)) => {
                        let placeholders: Vec<Message> = response
                            .tool_calls
                            .iter()
                            .map(|tc| {
                                let content = if tc.id == matched_id {
                                    if has_resume_host {
                                        format!(
                                            "[APPROVAL REQUESTED] Tool '{}' is waiting for human \
                                             approval (request {}).",
                                            tc.name, draft.request.request_id
                                        )
                                    } else {
                                        format!(
                                            "[APPROVAL REQUIRED] Tool '{}' requires human \
                                             approval, which is not available in this background \
                                             context. Do not retry.",
                                            tc.name
                                        )
                                    }
                                } else {
                                    format!(
                                        "[SKIPPED] Tool call suspended: this batch is waiting \
                                         for human approval of '{matched_name}'."
                                    )
                                };
                                tool_placeholder(&tc.id, content)
                            })
                            .collect();
                        if let Some(log) = turn_output_log {
                            log.push(assistant_msg);
                            log.extend(placeholders.iter().cloned());
                        }
                        messages.extend(placeholders);
                        if let Some(slot) = pending_approval_out {
                            tracing::info!(
                                tool = %matched_name,
                                request_id = %draft.request.request_id,
                                "suspending turn pending human approval"
                            );
                            *slot = Some(draft);
                        } else {
                            tracing::warn!(
                                tool = %matched_name,
                                "human-approval rule matched in a context without a resume \
                                 host; denying instead of suspending"
                            );
                        }
                        return Ok(response);
                    }
                    Ok(None) => {}
                    Err(err) => {
                        // Rule produced an invalid spec (config validation
                        // should prevent this). Fail the batch visibly
                        // without suspending.
                        let placeholders: Vec<Message> = response
                            .tool_calls
                            .iter()
                            .map(|tc| {
                                tool_placeholder(
                                    &tc.id,
                                    format!(
                                        "[APPROVAL POLICY ERROR] Tool '{matched_name}' matched \
                                         an invalid approval rule: {err}"
                                    ),
                                )
                            })
                            .collect();
                        if let Some(log) = turn_output_log {
                            log.push(assistant_msg);
                            log.extend(placeholders.iter().cloned());
                        }
                        messages.extend(placeholders);
                        return Ok(response);
                    }
                }
            }
        }

        let (limited_response, blocked_messages) =
            self.enforce_session_limits_on_tool_calls(&response);
        let tool_batches = split_tool_calls(
            &limited_response.tool_calls,
            MAX_PARALLEL_TOOL_CALLS_PER_BATCH,
        );
        if tool_batches.len() > 1 {
            tracing::info!(
                requested_tools = limited_response.tool_calls.len(),
                batch_size = MAX_PARALLEL_TOOL_CALLS_PER_BATCH,
                batches = tool_batches.len(),
                "capping parallel tool execution per turn"
            );
        }

        let mut tool_messages = Vec::new();
        let mut tool_files = Vec::new();
        let mut tool_send_files = Vec::new();
        let mut tool_tokens = TokenUsage::default();
        let mut tool_metadata: Vec<(String, serde_json::Value)> = Vec::new();
        // Codex round-2 MAJOR 2 (PR #1187 fixup): collect per-tool-call
        // success bits across every batch in this turn. Threaded out via
        // `tool_success_by_id` so the synth-ack gate can read the
        // authoritative `ToolResult.success` value rather than guessing
        // from content prefixes (which missed shell timeouts, sandbox
        // path rejections, browser nav failures, etc.).
        let mut tool_success: Vec<(String, bool)> = Vec::new();
        for batch in tool_batches {
            let mut batch_response = limited_response.clone();
            batch_response.tool_calls = batch.to_vec();
            let (
                batch_messages,
                batch_files,
                batch_send_files,
                batch_tokens,
                batch_metadata,
                batch_success,
            ) = self.execute_tools(&batch_response).await?;
            tool_messages.extend(batch_messages);
            tool_files.extend(batch_files);
            tool_send_files.extend(batch_send_files);
            tool_tokens.input_tokens += batch_tokens.input_tokens;
            tool_tokens.output_tokens += batch_tokens.output_tokens;
            tool_tokens.cache_read_tokens += batch_tokens.cache_read_tokens;
            tool_tokens.cache_write_tokens += batch_tokens.cache_write_tokens;
            tool_metadata.extend(batch_metadata);
            tool_success.extend(batch_success);
        }
        if let Some(terminal_tools) = terminal_tools_for_turn {
            let tool_name_by_id: HashMap<&str, &str> = limited_response
                .tool_calls
                .iter()
                .map(|call| (call.id.as_str(), call.name.as_str()))
                .collect();
            for (tool_call_id, metadata) in &tool_metadata {
                if metadata
                    .get("do_not_retry_same_turn")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)
                {
                    if let Some(tool_name) = tool_name_by_id.get(tool_call_id.as_str()) {
                        terminal_tools.insert((*tool_name).to_string());
                    }
                }
            }
        }
        if let Some(sink) = tool_structured_metadata {
            sink.extend(tool_metadata);
        }
        let tool_success_for_ledger = tool_success.clone();
        if let Some(sink) = tool_success_by_id {
            sink.extend(tool_success);
        }

        let mut merged = merge_tool_messages_in_order(
            &response,
            &limited_response,
            tool_messages,
            blocked_messages,
        );

        // PR #1363: OpenClaw-style no-progress check. For each Tool
        // message we just produced, record `(tool_name, args, result)`
        // in the result-aware ring. If the last 3 records match, append
        // a soft NO_PROGRESS hint to that tool message's content so the
        // LLM sees it on its next iteration. Distinguishes a stuck loop
        // (identical (args, result) repeated) from a legitimate poll
        // (same args, evolving result). Non-terminating — the hard
        // cycle detector at the caller's pre-call site is the backstop.
        {
            use std::collections::HashMap;
            let id_to_call: HashMap<&str, (&str, &serde_json::Value)> = response
                .tool_calls
                .iter()
                .map(|tc| (tc.id.as_str(), (tc.name.as_str(), &tc.arguments)))
                .collect();
            let success_by_id: HashMap<&str, bool> = tool_success_for_ledger
                .iter()
                .map(|(id, success)| (id.as_str(), *success))
                .collect();
            let stated_intent = response.content.as_deref();
            let mut turn_ledger = turn_ledger;
            for message in merged.iter_mut() {
                if message.role != MessageRole::Tool {
                    continue;
                }
                let Some(id) = message.tool_call_id.as_deref() else {
                    continue;
                };
                let Some(&(name, args)) = id_to_call.get(id) else {
                    continue;
                };
                let result_before_hint = message.content.clone();
                let repeating = if let Some(hint) =
                    loop_detector.record_result(name, args, &result_before_hint)
                {
                    message.content.push_str(&hint);
                    true
                } else {
                    false
                };
                if let Some(ledger) = turn_ledger.as_deref_mut() {
                    ledger.push_entry(ledger_entry_from_tool_result(
                        turn.iteration(),
                        stated_intent,
                        name,
                        args,
                        success_by_id.get(id).copied(),
                        &result_before_hint,
                        repeating,
                    ));
                }
            }
        }

        // M6.2: record a productive-tool-call signal per merged Tool message
        // so the `LoopRetryState` grace-call path sees the loop making progress.
        // A tool message counts as productive when it is neither an error
        // ("Error:" prefix), a panic, a timeout, nor a hook/session-limit
        // block — i.e. the tool produced output the LLM can act on.
        for message in &merged {
            if message.role == MessageRole::Tool && is_productive_tool_message(&message.content) {
                retry_state.record_productive_tool_call();
            }
        }

        // NEW-16: mirror the same assistant + merged rows into the
        // append-only turn log when the caller (conversation loop)
        // supplied a sink. The clone is intentional — the prompt
        // buffer `messages` will be mutated downstream by
        // `prepare_conversation_messages` /
        // `repair_message_order`, but the log must stay frozen as the
        // chronological record of what THIS turn produced.
        if let Some(log) = turn_output_log {
            log.push(assistant_msg);
            log.extend(merged.iter().cloned());
        }
        messages.extend(merged);
        files_modified.extend(tool_files);
        if let Some(files_to_send) = files_to_send {
            files_to_send.extend(tool_send_files);
        }
        turn.record_usage(
            tool_tokens.input_tokens,
            tool_tokens.output_tokens,
            tool_tokens.cache_read_tokens,
            tool_tokens.cache_write_tokens,
            tracker,
            // Tool-reported usage has no per-response provider attribution;
            // price it at the active slot as the closest estimate.
            self.response_usage_cost(tool_tokens.input_tokens, tool_tokens.output_tokens, None),
        );
        // Codex round-3: return the sanitized response so the caller's
        // synth-ack gate sees the SAME tool_call_ids that the success-bit
        // sink was keyed by. See doc-comment on this fn.
        Ok(response)
    }
}

/// Classify a tool-result `content` string as an error / denial / cancellation
/// emitted by the in-process tool dispatcher.
///
/// Mirrors the well-known conventions emitted by [`crate::agent::execution`]:
///
/// - `"Error: …"` — wrapper text added by `execute_tools` for any tool whose
///   `execute_with_context` call returned `Err`.
/// - `"[VALIDATION FAILED] …"` — spawn_only pre-flight rejection (the
///   `Tool::pre_flight_validate` hook returned `Err`).
/// - `"[POLICY DENIED] …"` / `"[HOOK DENIED] …"` — registry / lifecycle-hook
///   refusals at the call boundary.
/// - `"[SESSION LIMIT] …"` / `"[SHELL RETRY LIMIT] …"` — session-scoped
///   limiter refusals.
/// - `"Tool '<name>' panicked …"` / `"Tool '<name>' timed out …"` /
///   `"Tool '<name>' cancelled due to earlier sibling error …"` — synthetic
///   results minted by `panic_result` / the batch timeout path /
///   `cancelled_result`.
///
/// Used by the spawn_only branch in [`Agent::process_message_inner`] to
/// decide whether the synthesized "Background work started for `<tool>`."
/// acknowledgement is safe to emit. When any spawn_only tool the LLM called
/// produced one of these error-shaped results, the ack would otherwise sit
/// alongside the red error chip the UI already shows for the failed
/// invocation — a confusing dual signal (the fleet-UX soak symptom B4).
///
/// Returns `false` for the canonical spawn_only success placeholder
/// (`task_handle` envelope from `spawn_only_handle_message` /
/// `spawn_only_message`) and for every regular successful tool body, so the
/// detector never produces a false positive that suppresses the ack for a
/// genuinely-started background task.
fn is_error_tool_message(content: &str) -> bool {
    let trimmed = content.trim_start();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.starts_with("Error:")
        || trimmed.starts_with("[VALIDATION FAILED]")
        || trimmed.starts_with("[POLICY DENIED]")
        || trimmed.starts_with("[HOOK DENIED]")
        || trimmed.starts_with("[SESSION LIMIT]")
        || trimmed.starts_with("[SHELL RETRY LIMIT]")
    {
        return true;
    }
    if trimmed.starts_with("Tool '")
        && (trimmed.contains("panicked")
            || trimmed.contains("timed out")
            || trimmed.contains("cancelled due to earlier"))
    {
        return true;
    }
    false
}

/// Scan the tool-result messages appended during this turn for any tool
/// invocation (spawn_only or otherwise) that returned an error-shaped body.
///
/// Used by the spawn_only branch in [`Agent::process_message_inner`] to gate
/// the synthesized "Background work started for `<tool>`." acknowledgement.
/// When `true`, the ack is suppressed and the agent loop falls through to its
/// normal next-iteration path so the LLM observes the error tool result and
/// can react (acknowledge, retry, fall back, or surface the failure to the
/// user) instead of the harness fabricating a "started" confirmation
/// alongside the red error chip the UI already renders for the failed
/// tool — see the fleet-UX soak B4 finding (mini1 / dspfac, 2026-05-22).
///
/// The check spans EVERY tool call in the response (not just the spawn_only
/// ones) because the user-visible UX bug is the synth-ack rendering as a
/// success bubble while any sibling tool's red error chip is showing. The
/// LLM still has the next iteration to acknowledge / recover regardless of
/// which tool failed, so suppressing the ack here is strictly better UX.
///
/// Codex round-2 MAJOR 2 (PR #1187 fixup): the per-call `tool_success_by_id`
/// map is the AUTHORITATIVE signal. When the dispatcher reports
/// `success == false` for a tool_call_id present in the current response
/// we return `true` immediately, regardless of content shape. This catches
/// every legitimate failure mode whose tool body did NOT carry one of the
/// well-known error prefixes — shell timeouts ("Command timed out after
/// ..."), sandbox path rejections ("Path outside working directory ..."),
/// browser navigation failures, plugin tools returning `success: false`
/// with arbitrary error messages — every one of which renders a red error
/// chip but used to slip past the content-only classifier.
///
/// We retain the content-based fallback ([`is_error_tool_message`]) for
/// tool_call_ids that have NO entry in the success map. That covers
/// blocked-by-session-limit and other synthesised messages constructed
/// outside `execute_tools` (see `session_limit_message` /
/// `merge_tool_messages_in_order`) which never carry an executed `success`
/// bit but DO start with `[SESSION LIMIT]` / `[SHELL RETRY LIMIT]` so the
/// content classifier still gates them correctly.
fn any_tool_invocation_errored(
    messages: &[Message],
    response: &ChatResponse,
    tool_success_by_id: &[(String, bool)],
) -> bool {
    response.tool_calls.iter().any(|tc| {
        // Primary path: read the executed-tool success bit.
        if let Some((_, success)) = tool_success_by_id
            .iter()
            .find(|(id, _)| id.as_str() == tc.id)
        {
            return !*success;
        }
        // Fallback for tool_call_ids that bypassed `execute_tools` (e.g.
        // session-limit blocks emit a synthetic tool message via
        // `session_limit_message`). The dispatcher synthesises one Tool
        // message per tool_call_id, so a linear scan over recent messages
        // is bounded by the per-turn batch size
        // (≤ MAX_PARALLEL_TOOL_CALLS_PER_BATCH = 8 in production).
        messages.iter().rev().any(|message| {
            message.role == MessageRole::Tool
                && message
                    .tool_call_id
                    .as_deref()
                    .is_some_and(|id| id == tc.id)
                && is_error_tool_message(&message.content)
        })
    })
}

/// Classify a tool-result `content` string as productive for the M6.2
/// grace-call gating.
///
/// A productive result is a tool message whose body carries strong evidence
/// that the underlying tool actually accomplished useful work: either it
/// ended with an explicit success exit code or it returned a substantive
/// output block that is not one of the well-known error/denial conventions.
/// We apply a conservative lower bound (128 bytes of substantive output or
/// an explicit "Exit code: 0" marker) so that failure messages — which
/// `ToolResult { success: false }` tools tend to emit as short diagnostic
/// strings — do not accidentally keep a stalled loop alive past budget.
fn is_productive_tool_message(content: &str) -> bool {
    let trimmed = content.trim_start();
    if trimmed.is_empty() {
        return false;
    }
    // Never productive: explicit error/denial conventions.
    if trimmed.starts_with("Error:")
        || trimmed.starts_with("[HOOK DENIED]")
        || trimmed.starts_with("[SESSION LIMIT]")
        || trimmed.starts_with("[SHELL RETRY LIMIT]")
        || trimmed.starts_with("Path outside working directory")
        || trimmed.starts_with("(no output)")
        || trimmed.starts_with("File not found")
        || (trimmed.starts_with("Tool '")
            && (trimmed.contains("panicked") || trimmed.contains("timed out")))
    {
        return false;
    }

    // Positive: explicit shell success exit code.
    if trimmed.contains("\nExit code: 0") || trimmed.ends_with("Exit code: 0") {
        return true;
    }

    // Conservative fallback: require a substantive body. Short failure
    // messages like "File too large..." or "Symlinks are not allowed" fall
    // under this bound so they never inflate the productive counter.
    trimmed.len() >= 128 && !trimmed.to_ascii_lowercase().contains("failed to")
}

fn check_per_tool_limit(
    usage: &crate::session::SessionUsage,
    tool_name: &str,
    limits: &SessionLimits,
) -> bool {
    limits
        .per_tool_limits
        .get(tool_name)
        .is_none_or(|max_calls| usage.tool_calls.get(tool_name).copied().unwrap_or(0) < *max_calls)
}

fn session_limit_message(tool_call: &octos_core::ToolCall, content: String) -> Message {
    Message {
        role: MessageRole::Tool,
        content,
        media: vec![],
        tool_calls: None,
        tool_call_id: Some(tool_call.id.clone()),
        reasoning_content: None,
        client_message_id: None,
        thread_id: None,
        timestamp: chrono::Utc::now(),
    }
}

fn merge_tool_messages_in_order(
    original_response: &ChatResponse,
    limited_response: &ChatResponse,
    executed_messages: Vec<Message>,
    blocked_messages: Vec<Message>,
) -> Vec<Message> {
    if blocked_messages.is_empty() {
        return executed_messages;
    }

    let mut executed_by_id: VecDeque<Message> = executed_messages.into();
    let blocked_by_id: HashMap<String, Message> = blocked_messages
        .into_iter()
        .filter_map(|message| message.tool_call_id.clone().map(|id| (id, message)))
        .collect();

    let allowed_ids: std::collections::HashSet<&str> = limited_response
        .tool_calls
        .iter()
        .map(|tool_call| tool_call.id.as_str())
        .collect();

    let mut ordered = Vec::new();
    for tool_call in &original_response.tool_calls {
        if !allowed_ids.contains(tool_call.id.as_str()) {
            if let Some(message) = blocked_by_id.get(&tool_call.id) {
                ordered.push(message.clone());
            }
            continue;
        }
        if let Some(message) = executed_by_id.pop_front() {
            ordered.push(message);
        }
    }
    ordered.extend(executed_by_id);
    ordered
}

fn recover_shell_retry(
    messages: &[Message],
    min_shell_streak: usize,
) -> Option<ShellRetryRecovery> {
    let recent = recent_tool_results(messages, min_shell_streak * 3);
    let shell_results: Vec<&str> = recent
        .iter()
        .filter(|(tool_name, _)| *tool_name == "shell")
        .map(|(_, content)| content.as_str())
        .collect();

    if shell_results.len() < min_shell_streak {
        return None;
    }

    let failed_shells = shell_results
        .iter()
        .filter(|content| !is_successful_shell_output(content))
        .count();

    // Every recovery arm below is gated on `failed_shells` because this is a
    // shell-SPIRAL detector: it only intervenes when the model is stuck
    // retrying failing shell calls, not when it is deliberately running a
    // streak of successful commands. DiffLikeSuccess must be gated too —
    // otherwise a legitimate turn that runs `git diff`/`git show` >= threshold
    // times (all exit 0) is short-circuited, surfacing the raw diff as the
    // assistant answer and terminating the turn before the model can reply. A
    // genuine stuck-on-identical-success loop is caught by the separate
    // loop-detector, not here. `>= 1` mirrors the UsefulSuccess sibling.
    (failed_shells >= 1)
        .then(|| {
            shell_results
                .iter()
                .find(|content| is_diff_like_shell_output(content))
        })
        .flatten()
        .map(|content| ShellRetryRecovery {
            kind: ShellRetryRecoveryKind::DiffLikeSuccess,
            content: strip_success_exit_suffix(content),
        })
        .or_else(|| {
            (failed_shells >= 2)
                .then(|| {
                    shell_results
                        .iter()
                        .find(|content| is_validation_like_shell_output(content))
                })
                .flatten()
                .map(|content| ShellRetryRecovery {
                    kind: ShellRetryRecoveryKind::ValidationSuccess,
                    content: strip_success_exit_suffix(content),
                })
        })
        .or_else(|| {
            (failed_shells >= 1)
                .then(|| {
                    shell_results
                        .iter()
                        .find(|content| is_recoverable_non_diff_shell_output(content))
                })
                .flatten()
                .map(|content| ShellRetryRecovery {
                    kind: ShellRetryRecoveryKind::UsefulSuccess,
                    content: strip_success_exit_suffix(content),
                })
        })
        .or_else(|| {
            // A retry spiral means the model re-runs the SAME command against
            // the same failure. Agentic models (kimi k3) legitimately fan out
            // many DISTINCT commands per turn where several exit non-zero
            // (grep with no match exits 1) — counting any N failures killed
            // those turns mid-task, so the failures must also concentrate on
            // one repeated command (or one repeated failure text) before the
            // limit fires.
            (failed_shells >= min_shell_streak.saturating_sub(1)
                && max_identical_failed_shell_signature(messages, min_shell_streak * 3)
                    >= min_shell_streak.saturating_sub(1))
            .then(|| shell_results.first().copied())
            .flatten()
            .map(|content| ShellRetryRecovery {
                kind: ShellRetryRecoveryKind::RetryLimit,
                content: shell_retry_limit_message(content),
            })
        })
}

/// Strongest "not converging" repeat signal among FAILING shell calls in the
/// most recent `limit` tool results: the max of
///
///  - identical commands (whitespace-normalized) — the model re-runs the same
///    command against the same failure (timeout retries), and
///  - identical non-empty failure texts (output minus the exit-code suffix) —
///    the model shuffles flags but keeps hitting the same wall (`cargo test`
///    → `cargo test --all` → … all "could not find Cargo.toml").
///
/// Distinct failing commands with distinct (or empty — a no-match grep) error
/// texts each count 1 and never reach the spiral threshold.
fn max_identical_failed_shell_signature(messages: &[Message], limit: usize) -> usize {
    let mut command_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut failure_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut seen = 0usize;
    for idx in (0..messages.len()).rev() {
        let message = &messages[idx];
        if message.role != MessageRole::Tool {
            continue;
        }
        let Some(tool_name) = resolve_tool_name(messages, idx) else {
            continue;
        };
        seen += 1;
        if tool_name == "shell" && !is_successful_shell_output(&message.content) {
            if let Some(key) = resolve_shell_command_key(messages, idx) {
                *command_counts.entry(key).or_default() += 1;
            }
            let failure_text = strip_exit_code_suffix(&message.content);
            if !failure_text.is_empty() {
                *failure_counts.entry(failure_text).or_default() += 1;
            }
        }
        if seen >= limit {
            break;
        }
    }
    command_counts
        .values()
        .chain(failure_counts.values())
        .copied()
        .max()
        .unwrap_or(0)
}

/// Identity key for the shell command behind the Tool message at
/// `tool_msg_index`: the `command` argument with whitespace collapsed, or the
/// full arguments JSON for non-standard shapes (so identical repeats still
/// match).
fn resolve_shell_command_key(messages: &[Message], tool_msg_index: usize) -> Option<String> {
    let tool_call_id = messages.get(tool_msg_index)?.tool_call_id.as_deref()?;
    messages[..tool_msg_index].iter().rev().find_map(|message| {
        if message.role != MessageRole::Assistant {
            return None;
        }
        message.tool_calls.as_ref().and_then(|tool_calls| {
            tool_calls
                .iter()
                .find(|tool_call| tool_call.id == tool_call_id)
                .map(|tool_call| {
                    tool_call
                        .arguments
                        .get("command")
                        .and_then(|value| value.as_str())
                        .map(|command| command.split_whitespace().collect::<Vec<_>>().join(" "))
                        .unwrap_or_else(|| tool_call.arguments.to_string())
                })
        })
    })
}

/// Failure text of a shell output: the content with any trailing
/// `Exit code: N` line removed and whitespace trimmed. Empty for outputs
/// that carry nothing but the exit code — including the shell tool's
/// "(no output)" sentinel (shell.rs renders empty stdout+stderr as
/// literally "(no output)\n\nExit code: N"), so several DISTINCT no-match
/// probes all sharing that sentinel count as no failure text at all and
/// never concentrate into a spiral signature.
fn strip_exit_code_suffix(content: &str) -> String {
    let trimmed = content.trim_end();
    let without_exit = match trimmed.rsplit_once("Exit code:") {
        Some((head, tail)) if tail.trim().chars().all(|ch| ch.is_ascii_digit()) => head,
        _ => trimmed,
    };
    let failure = without_exit.trim();
    if failure == "(no output)" {
        String::new()
    } else {
        failure.to_string()
    }
}

fn recent_tool_results(messages: &[Message], limit: usize) -> Vec<(String, String)> {
    let mut results = Vec::new();

    for idx in (0..messages.len()).rev() {
        let message = &messages[idx];
        if message.role != MessageRole::Tool {
            continue;
        }
        let Some(tool_name) = resolve_tool_name(messages, idx) else {
            continue;
        };
        results.push((tool_name.to_string(), message.content.clone()));
        if results.len() >= limit {
            break;
        }
    }

    results
}

fn resolve_tool_name(messages: &[Message], tool_msg_index: usize) -> Option<&str> {
    let tool_call_id = messages.get(tool_msg_index)?.tool_call_id.as_deref()?;

    messages[..tool_msg_index].iter().rev().find_map(|message| {
        if message.role != MessageRole::Assistant {
            return None;
        }
        message.tool_calls.as_ref().and_then(|tool_calls| {
            tool_calls
                .iter()
                .find(|tool_call| tool_call.id == tool_call_id)
                .map(|tool_call| tool_call.name.as_str())
        })
    })
}

/// Outcome of `dispatch_shell_retry_recovery`. The caller branches on
/// `(recovery.kind, decision)`:
///
///  - `(RetryLimit, Escalate)` → first spiral hit on a non-converging
///    streak. Splice `recovery.content` (system-shaped instruction) into
///    the latest Tool message and continue the loop so the LLM gets one
///    iteration to produce a real user-facing summary.
///  - `(RetryLimit, Exhausted)` → second spiral hit; the model already
///    had its summary chance and ignored it. Terminate the turn with
///    `recovery.content` as the assistant reply (the system-shaped string
///    is at least better than another infinite loop).
///  - `(DiffLikeSuccess | ValidationSuccess | UsefulSuccess, _)` →
///    `recovery.content` is RAW shell output extracted from the
///    spiraling noise. It IS useful as a user-facing reply; keep the
///    original return-as-content path. Do NOT splice — that would
///    mis-attribute older successful output to the latest shell call.
pub(crate) struct ShellSpiralOutcome {
    pub(crate) recovery: ShellRetryRecovery,
    pub(crate) decision: LoopDecision,
}

/// Index of the most recent `MessageRole::User` message in `messages`,
/// or `0` if there is no User message yet (e.g. early agent boot). The
/// returned index marks the start of the current user turn — anything
/// before it belongs to past turns and is OUT OF SCOPE for the
/// shell-spiral detector.
fn current_user_turn_start(messages: &[Message]) -> usize {
    messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(idx, msg)| (msg.role == MessageRole::User).then_some(idx))
        .unwrap_or(0)
}

/// Walk backward from the end of `messages` collecting names attached to
/// the trailing run of Tool messages (the "latest tool batch"). Returns
/// true if any of those names matches `target`. Returns false if the
/// trailing run is empty (no Tool message at the tail) or none of the
/// resolved names match.
///
/// Multi-tool batch awareness: the LLM can emit several tool calls in a
/// single response (`[shell, read_file]`), and they are appended to
/// messages as a contiguous run of Tool entries. Gating on only the
/// LATEST one would suppress legitimate shell-spiral detection just
/// because a non-shell tool happened to be appended last.
fn latest_tool_batch_contains(messages: &[Message], target: &str) -> bool {
    latest_tool_batch_index(messages, target).is_some()
}

/// Index of the most recent Tool message in the trailing batch whose
/// resolved tool name is `target`, or `None` if the trailing batch
/// contains no such Tool. Mirrors the walk in
/// `latest_tool_batch_contains` but returns the index so callers can
/// mutate that specific message.
///
/// Used by the spiral-recovery splice path: when a `[shell, read_file]`
/// batch trips the detector, the recovery notice must overwrite the
/// SHELL Tool's content, not whichever Tool happened to be appended
/// last. Otherwise we mis-attribute the system-shaped instruction to
/// `read_file` and silently drop the actual shell output that the
/// notice is supposed to reference.
fn latest_tool_batch_index(messages: &[Message], target: &str) -> Option<usize> {
    for idx in (0..messages.len()).rev() {
        let msg = &messages[idx];
        if msg.role != MessageRole::Tool {
            return None;
        }
        if resolve_tool_name(messages, idx) == Some(target) {
            return Some(idx);
        }
    }
    None
}

/// Sanitize the system-shaped `[SHELL RETRY LIMIT]` content for the
/// terminal Exhausted path so the user-facing assistant reply isn't a
/// raw LLM-instruction string. Strips the fixed prefix that
/// `shell_retry_limit_message` prepends and wraps the latest shell
/// output in a short user-readable framing.
///
/// Codex round-3 BLOCK: the prefix can NEST. After the Escalate splice
/// overwrites a shell Tool's content with `[SHELL RETRY LIMIT] ... +
/// original output`, a follow-up recovery wraps that already-prefixed
/// content again, producing two layers of the system prefix. We strip
/// recursively until no prefix remains so the user-facing assistant
/// reply never leaks an inner `[SHELL RETRY LIMIT] ... Stop retrying
/// shell ...` instruction.
fn shell_retry_terminal_user_message(content: &str) -> String {
    const PREFIX: &str = "[SHELL RETRY LIMIT] Repeated shell repair attempts did not converge. Stop retrying shell and summarize the blocker.\n\nLatest shell output:\n";
    let mut tail = content;
    while let Some(stripped) = tail.strip_prefix(PREFIX) {
        tail = stripped;
    }
    if tail.trim().is_empty() {
        "I tried multiple shell approaches but couldn't converge on an answer. Please rephrase or give me a more specific direction.".to_string()
    } else {
        format!(
            "I tried multiple shell approaches but couldn't converge on an answer. Latest output:\n\n{tail}"
        )
    }
}

/// Inject a synthetic conversation pair when the loop detector fires for the
/// FIRST time in a turn so the LLM gets the chance to course-correct.
///
/// Specifically:
///   1. Push the looping assistant message (with its `tool_calls`).
///   2. For EVERY tool call in the response, push a matching tool-result
///      message — provider chat schemas require a 1:1 pairing.
///   3. The FIRST tool-result carries `warning` (the loop-detector text +
///      synthesis hint). Companion tool calls in the same response get a
///      short stub so the LLM doesn't think they actually executed.
///
/// We never call the tools — the looping calls would just produce more
/// drifted output. The synthesis hint tells the LLM to fall back to prior
/// results already in the conversation or switch tools.
///
/// See PR `fix/news-fetch-loop-and-detect-recovery`
/// (session `web-1779494658716-mxrxe8`, ledger seq 214-562).
///
/// NEW-16: kept alive for the test suite which exercises the legacy
/// no-log API. Production callers go through
/// `inject_loop_detected_synthetic_results_with_log`.
#[cfg(test)]
fn inject_loop_detected_synthetic_results(
    messages: &mut Vec<Message>,
    response: &ChatResponse,
    warning: &str,
    agent: &Agent,
) {
    inject_loop_detected_synthetic_results_with_log(messages, response, warning, agent, None);
}

/// NEW-16: same as `inject_loop_detected_synthetic_results`, but also
/// mirrors the synthetic assistant + tool rows into the conversation
/// loop's append-only `turn_output_log` when supplied. Keeps the
/// `messages` mutation behaviour byte-identical for callers that pass
/// `None` (tests in particular).
fn inject_loop_detected_synthetic_results_with_log(
    messages: &mut Vec<Message>,
    response: &ChatResponse,
    warning: &str,
    agent: &Agent,
    turn_output_log: Option<&mut Vec<Message>>,
) {
    let synthesis_hint = "\n\nTry a different approach — synthesise from prior tool results already in this conversation, call a different tool, or finish the turn with the partial information you have.";
    let primary_body = format!("{warning}{synthesis_hint}");
    let stub_body =
        "[LOOP DETECTED] (companion call in the same batch; see paired result for the warning).";

    // Sanitize tool_call_ids the same way the normal `handle_tool_use` path
    // does (see loop_runner.rs line ~1685): some providers (Moonshot/kimi)
    // emit IDs containing colons like "admin_view_sessions:11" which OpenAI
    // and our duplicate-repair logic both reject/collapse. Skipping this on
    // the synthetic path would leave the next LLM call with unanswered
    // tool_calls or a 400 from the next request. We sanitize on a clone of
    // the response so the SAME id flows into BOTH the assistant message's
    // `tool_calls` (via `response_to_message`) and the matching tool-result
    // `tool_call_id` below, preserving the 1:1 pairing end-to-end.
    let mut sanitized_response = response.clone();
    for tc in sanitized_response.tool_calls.iter_mut() {
        tc.id = sanitize_tool_call_id(&tc.id);
    }

    // Push the assistant turn (carries the sanitized `tool_calls`) so the
    // synthetic tool-result messages have a corresponding `tool_use` to
    // bind to.
    let assistant_msg = agent.response_to_message(&sanitized_response);
    messages.push(assistant_msg.clone());
    // Collect the same rows we just pushed so we can mirror them into
    // the append-only turn log below (when a sink was supplied).
    let mut rows_for_log: Vec<Message> =
        Vec::with_capacity(1 + sanitized_response.tool_calls.len());
    rows_for_log.push(assistant_msg);

    for (idx, tc) in sanitized_response.tool_calls.iter().enumerate() {
        let body = if idx == 0 { &primary_body } else { stub_body };
        let tool_msg = Message {
            role: MessageRole::Tool,
            content: body.to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: Some(tc.id.clone()),
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        };
        messages.push(tool_msg.clone());
        rows_for_log.push(tool_msg);
    }

    if let Some(log) = turn_output_log {
        log.extend(rows_for_log);
    }
}

/// #1765: terminal message returned when the doom-loop guard fires —
/// the model issued the same tool call (same name + identical arguments
/// JSON) [`crate::loop_detect::DOOM_LOOP_THRESHOLD`]+ times in a row.
/// The text is both model-facing (it lands in session history, so the
/// model can change course next turn) and user-facing (it is the
/// assistant reply for the aborted turn).
fn doom_loop_terminal_message(tool_name: &str, streak: usize) -> String {
    format!(
        "[DOOM LOOP DETECTED] The `{tool_name}` tool was called {streak} times in a row \
         with identical arguments, so this turn was stopped before issuing another model \
         call. Repeating the exact same call cannot produce a different result. Try a \
         different approach — vary the arguments, use a different tool, or rephrase the \
         request."
    )
}

/// A tool that has already exhausted its own retries can mark the failure as
/// terminal for the current user turn. If the model asks for it again, stop
/// before execution even when it rewrites the arguments. This message is
/// intentionally free of internal error details: the first tool result
/// already carries those details in the transcript and the user only needs
/// to know that the repeated wait was prevented.
fn terminal_tool_retry_message(tool_name: &str) -> String {
    format!(
        "The requested operation already exhausted its internal retries. I stopped a repeated '{tool_name}' call in this turn so you do not have to wait for the same work again. Please send a new message if you want to retry."
    )
}

/// Terminal message returned when the LLM ignores the loop-detector
/// warning and trips the detector a SECOND time in the same turn.
fn loop_detected_terminal_message() -> String {
    "[LOOP DETECTED] The agent kept calling the same tool with the same arguments \
     even after a warning was injected. Stopping the turn to avoid a thrash. \
     Please rephrase your request or try a different angle."
        .to_string()
}

fn is_useful_shell_output(content: &str) -> bool {
    let trimmed = content.trim();
    content.contains("Exit code: 0")
        && !trimmed.is_empty()
        && trimmed != "Exit code: 0"
        && !trimmed.starts_with("(no output)")
}

/// Tier-1 pass for this iteration (spec kv-cache-friendly-compaction): the
/// turn's first call may rewrite deep history (stale pruning); later
/// iterations only clear oversized results near the prefix tail, keeping the
/// provider prefix cache (KV) valid across the turn's iterations.
fn tier1_pass(iteration: u32) -> crate::compaction_tiered::Tier1Pass {
    if iteration == 1 {
        crate::compaction_tiered::Tier1Pass::Full
    } else {
        crate::compaction_tiered::Tier1Pass::OversizedOnly
    }
}

fn is_successful_shell_output(content: &str) -> bool {
    content.contains("Exit code: 0")
}

fn is_diff_like_shell_output(content: &str) -> bool {
    is_useful_shell_output(content)
        && (content.contains("diff --git")
            || (content.contains("\n--- ") && content.contains("\n+++ "))
            || content.contains("\n@@ "))
}

fn is_validation_like_shell_output(content: &str) -> bool {
    is_useful_shell_output(content)
        && [
            "test result: ok",
            "0 failed",
            "All tests passed",
            "BUILD SUCCESS",
            "build succeeded",
            "Tests passed",
            "PASS ",
            " passed in ",
            " passing",
        ]
        .iter()
        .any(|marker| content.contains(marker))
}

fn is_recoverable_non_diff_shell_output(content: &str) -> bool {
    is_useful_shell_output(content) && content.lines().any(is_git_status_short_line)
}

fn is_git_status_short_line(line: &str) -> bool {
    let line = line.trim_end();
    let bytes = line.as_bytes();
    if bytes.len() < 4 || !bytes[2].is_ascii_whitespace() {
        return false;
    }

    let status = &line[..2];
    let has_status = status.chars().any(|ch| ch != ' ');
    let valid_status = status
        .chars()
        .all(|ch| matches!(ch, ' ' | 'M' | 'A' | 'D' | 'R' | 'C' | 'U' | '?' | '!'));
    has_status && valid_status && !line[3..].trim().is_empty()
}

fn strip_success_exit_suffix(content: &str) -> String {
    content
        .strip_suffix("\n\nExit code: 0")
        .unwrap_or(content)
        .to_string()
}

fn push_max_tokens_continuation(messages: &mut Vec<Message>, response: &ChatResponse) {
    let mut assistant = Message::assistant(response.content.clone().unwrap_or_default());
    assistant.reasoning_content = response.reasoning_content.clone();
    messages.push(assistant);
    messages.push(Message::user(MAX_TOKENS_CONTINUATION_PROMPT));
}

fn response_with_max_token_fragments(
    response: &ChatResponse,
    fragments: &[String],
) -> ChatResponse {
    if fragments.is_empty() {
        return response.clone();
    }

    let mut combined_parts: Vec<&str> = fragments
        .iter()
        .map(String::as_str)
        .filter(|part| !part.trim().is_empty())
        .collect();
    let final_content = response.content.as_deref().unwrap_or_default();
    if !final_content.trim().is_empty() {
        combined_parts.push(final_content);
    }

    let mut combined = response.clone();
    combined.content = Some(combined_parts.join("\n"));
    combined
}

fn shell_retry_limit_message(content: &str) -> String {
    let latest_output =
        octos_core::truncated_utf8(content.trim(), 1200, "\n... (shell output truncated)");
    format!(
        "[SHELL RETRY LIMIT] Repeated shell repair attempts did not converge. Stop retrying shell and summarize the blocker.\n\nLatest shell output:\n{latest_output}"
    )
}

#[cfg(test)]
#[path = "loop_runner_tests.rs"]
mod tests;
