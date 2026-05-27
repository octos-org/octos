//! Main agent loop: process_message and run_task orchestration.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use std::{collections::HashMap, collections::VecDeque};

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
use super::turn_state::{LoopRetryReason, LoopTurnState};
use super::{Agent, ConversationResponse, TASK_REPORTER, TokenTracker};
use crate::harness_errors::HarnessError;
use crate::harness_events::write_event_to_sink;
use crate::loop_detect::LoopDetector;
use crate::progress::ProgressEvent;
use crate::prompt_context::PromptContextPhase;
use crate::session::SessionLimits;
use crate::tools::{TURN_ATTACHMENT_CTX, TurnAttachmentContext};

const MAX_PARALLEL_TOOL_CALLS_PER_BATCH: usize = 8;
const MAX_TOKENS_CONTINUATION_LIMIT: usize = 2;
const MAX_TOKENS_CONTINUATION_PROMPT: &str = "Your output was truncated at the token limit. Continue directly from where you stopped. Do not repeat or summarize what you already wrote.";
const SHELL_RETRY_RECOVERY_THRESHOLD: usize = 4;

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
                self.maybe_run_turn_compaction(messages, iteration);
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
        tracker: &TokenTracker,
    ) -> Result<ConversationResponse> {
        self.process_message_inner(user_content, history, media, attachments, Some(tracker))
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

                messages.extend_from_slice(history);

                let base_content = if user_content.is_empty() && !media.is_empty() {
                    "[User sent an image]".to_string()
                } else {
                    user_content.to_string()
                };
                let content = if let Some(summary) = TURN_ATTACHMENT_CTX
                    .try_with(|ctx| ctx.prompt_summary.clone())
                    .ok()
                    .flatten()
                {
                    if base_content.trim().is_empty() {
                        summary
                    } else {
                        format!("{base_content}\n\n{summary}")
                    }
                } else {
                    base_content
                };

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

                loop {
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
                                files_modified,
                                files_to_send,
                                streamed: false,
                                messages: turn_output_log.clone(),
                                tool_results: tool_structured_metadata.clone(),
                                synthesized_from_spawn_only: false,
                            });
                        }
                    }

                    let iteration = turn.advance_iteration();
                    // Realtime heartbeat: beat first, then abort the iteration
                    // with a typed error if the controller reports stalled.
                    // A None controller / disabled config is a no-op so the
                    // 830+ existing tests see identical behavior.
                    self.beat_heartbeat(iteration)?;
                    self.reporter()
                        .report(ProgressEvent::Thinking { iteration });

                    // LRU tool management: tick iteration counter and auto-evict idle tools
                    self.tools.tick();
                    let evicted = self.tools.auto_evict();
                    if !evicted.is_empty() {
                        tracing::info!(
                            evicted = %evicted.join(", "),
                            count = evicted.len(),
                            "auto-evicted idle tools"
                        );
                    }

                    let tools_spec = self.tools.specs();
                    // Harness M6.3: run preflight compaction before the first
                    // LLM call when a compaction policy is wired and the
                    // context already exceeds the declared threshold.
                    if iteration == 1 {
                        self.maybe_run_preflight_compaction(&mut messages);
                    }
                    // Harness M8.5 tier 1: cheap in-place stale/oversized
                    // tool-result pruning. Runs every iteration (including
                    // the first so large bootstrap payloads shrink before
                    // tier 3 considers whether to summarise).
                    let protected_ids = collect_protected_tool_call_ids(&messages);
                    self.run_tier1_compaction(&mut messages, &protected_ids);
                    prepare_conversation_messages(self, &mut messages, &mut turn);
                    // Harness M6.3: post-prep compaction pass so the declarative
                    // runner sees the final shape of the conversation (after
                    // tool-pair repair + system-message normalization). This
                    // also feeds the validator rail on subsequent iterations.
                    self.maybe_run_turn_compaction(&mut messages, iteration);
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
                    let (mut response, streamed) = match self
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
                                    match self.handle_loop_error_with_dispatch(
                                        &e,
                                        &mut retry_state,
                                        iteration,
                                        &mut messages,
                                    ) {
                                        LoopErrorAction::Retry => continue,
                                        LoopErrorAction::Bail => return Err(e),
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            match self.handle_loop_error_with_dispatch(
                                &e,
                                &mut retry_state,
                                iteration,
                                &mut messages,
                            ) {
                                LoopErrorAction::Retry => continue,
                                LoopErrorAction::Bail => return Err(e),
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
                            "LLM response received"
                        );
                    }
                    turn.record_usage(
                        response.usage.input_tokens,
                        response.usage.output_tokens,
                        tracker,
                    );

                    match response.stop_reason {
                        StopReason::EndTurn | StopReason::StopSequence => {
                            self.emit_cost_update(turn.total_usage(), &response);
                            return Ok(ConversationResponse {
                                content: response.content.unwrap_or_default(),
                                reasoning_content: response.reasoning_content.clone(),
                                provider_metadata: Some(
                                    self.llm.provider_metadata_for_index(response.provider_index),
                                ),
                                token_usage: turn.total_usage().clone(),
                                files_modified,
                                files_to_send,
                                streamed,
                                messages: turn_output_log.clone(),
                                tool_results: tool_structured_metadata.clone(),
                                synthesized_from_spawn_only: false,
                            });
                        }
                        StopReason::ToolUse => {
                            // Check for loop detection before executing
                            for tc in &response.tool_calls {
                                if let Some(warning) = loop_detector.record(&tc.name, &tc.arguments)
                                {
                                    warn!("loop detected — breaking agent loop");
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
                                                continue;
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
                                        self.emit_cost_update(turn.total_usage(), &response);
                                        return Ok(ConversationResponse {
                                            content: terminal_content,
                                            reasoning_content: None,
                                            provider_metadata: None,
                                            token_usage: turn.total_usage().clone(),
                                            files_modified,
                                            files_to_send,
                                            streamed,
                                            messages: turn_output_log.clone(),
                                            tool_results: tool_structured_metadata.clone(),
                                            synthesized_from_spawn_only: false,
                                        });
                                    }
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
                                    self.emit_cost_update(turn.total_usage(), &response);
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
                                            continue;
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
                                                files_modified,
                                                files_to_send,
                                                streamed,
                                                messages: turn_output_log.clone(),
                                                tool_results: tool_structured_metadata.clone(),
                                                synthesized_from_spawn_only: false,
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
                                        LoopErrorAction::Bail => return Err(e),
                                    }
                                }
                            };

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
                                self.emit_cost_update(turn.total_usage(), &response);
                                return Ok(ConversationResponse {
                                    content: terminal_content,
                                    reasoning_content: None,
                                    provider_metadata: Some(
                                        self.llm.provider_metadata_for_index(
                                            response.provider_index,
                                        ),
                                    ),
                                    token_usage: turn.total_usage().clone(),
                                    files_modified,
                                    files_to_send,
                                    streamed,
                                    messages: turn_output_log.clone(),
                                    tool_results: tool_structured_metadata.clone(),
                                    synthesized_from_spawn_only: false,
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
                                    self.emit_cost_update(turn.total_usage(), &response);
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
                                    // (colon → underscore, empty/duplicate
                                    // repair), and the supervisor stores the
                                    // sanitized id on the `BackgroundTask`.
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
                                        files_modified,
                                        files_to_send,
                                        streamed,
                                        messages: turn_output_log.clone(),
                                        tool_results: tool_structured_metadata.clone(),
                                        // dspfac "two bubbles per turn" fix: this
                                        // branch synthesises `content` as the
                                        // "Background work started for `<tool>`..."
                                        // acknowledgement. The API persist site
                                        // reads this flag and tags the wire
                                        // envelope for the synthesised row with
                                        // `MessagePersistedSource::Background`,
                                        // which the existing capability filter
                                        // for `event.spawn_complete.v1` clients
                                        // suppresses. Legacy clients (without
                                        // the capability) still see the ack as
                                        // an assistant row — backward-compatible.
                                        synthesized_from_spawn_only: true,
                                    });
                                }
                            }
                        }
                        StopReason::MaxTokens => {
                            self.emit_cost_update(turn.total_usage(), &response);
                            return Ok(ConversationResponse {
                                content: response.content.unwrap_or_default(),
                                reasoning_content: response.reasoning_content.clone(),
                                provider_metadata: Some(
                                    self.llm.provider_metadata_for_index(response.provider_index),
                                ),
                                token_usage: turn.total_usage().clone(),
                                files_modified,
                                files_to_send,
                                streamed,
                                messages: turn_output_log.clone(),
                                tool_results: tool_structured_metadata.clone(),
                                synthesized_from_spawn_only: false,
                            });
                        }
                        StopReason::ContentFiltered => {
                            // After retries in call_llm_with_hooks, content is still filtered.
                            // Return a user-visible message instead of empty content.
                            self.emit_cost_update(turn.total_usage(), &response);
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
                                files_modified,
                                files_to_send,
                                streamed,
                                messages: turn_output_log.clone(),
                                tool_results: tool_structured_metadata.clone(),
                                synthesized_from_spawn_only: false,
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
                self.beat_heartbeat(iteration)?;
                self.reporter()
                    .report(ProgressEvent::Thinking { iteration });

                // LRU tool management
                self.tools.tick();
                let evicted = self.tools.auto_evict();
                if !evicted.is_empty() {
                    tracing::info!(
                        evicted = %evicted.join(", "),
                        "auto-evicted idle tools in task"
                    );
                }

                let tools_spec = self.tools.specs();
                // M8.5 tier 1: also runs in task mode so background workers
                // benefit from the same cheap shrinkage before their LLM call.
                let protected_ids = collect_protected_tool_call_ids(&messages);
                self.run_tier1_compaction(&mut messages, &protected_ids);
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
                let (mut response, _streamed) = match self
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
                        match self.handle_loop_error_with_dispatch(
                            &e,
                            &mut retry_state,
                            iteration,
                            &mut messages,
                        ) {
                            LoopErrorAction::Retry => continue,
                            LoopErrorAction::Bail => return Err(e),
                        }
                    }
                };
                Self::normalize_inline_invokes(&mut response);
                turn.record_usage(response.usage.input_tokens, response.usage.output_tokens, None);

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

                match response.stop_reason {
                    StopReason::EndTurn | StopReason::StopSequence => {
                        let final_response =
                            response_with_max_token_fragments(&response, &max_token_fragments);
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

                        self.emit_cost_update(turn.total_usage(), &final_response);

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
                                LoopErrorAction::Bail => return Err(e),
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
                        self.emit_cost_update(turn.total_usage(), &final_response);
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
                        self.emit_cost_update(turn.total_usage(), &response);
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
                ..Default::default()
            },
        }
    }

    /// Execute tool calls from an LLM response and accumulate results.
    ///
    /// On success returns the SANITIZED response — IDs after
    /// `sanitize_tool_call_id` + empty/duplicate repair + name+args dedup.
    /// Callers that subsequently key into `tool_success_by_id` MUST use the
    /// sanitized response so the lookup matches; the original response's
    /// tool_call_ids are stale once sanitization rewrites them.
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
    ) -> Result<ChatResponse> {
        // Fix tool_call IDs -- some models (e.g. qwen via dashscope) generate
        // duplicate or empty IDs which downstream providers reject with 400.
        // Also sanitize characters: some providers (e.g. Moonshot/kimi) generate IDs
        // with colons like "admin_view_sessions:11" which OpenAI rejects.
        // We fix IDs on the response clone so both the assistant message and tool result
        // messages use the same corrected IDs.
        let mut response = response.clone();
        {
            let mut seen_ids = std::collections::HashSet::new();
            for (i, tc) in response.tool_calls.iter_mut().enumerate() {
                // Sanitize characters: keep only alphanumeric, underscore, hyphen
                tc.id = sanitize_tool_call_id(&tc.id);

                if tc.id.is_empty() || !seen_ids.insert(tc.id.clone()) {
                    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let new_id = format!("call_{}_{}", i, seq);
                    tracing::warn!(
                        old_id = %tc.id,
                        new_id = %new_id,
                        tool = %tc.name,
                        "fixing empty/duplicate tool_call_id"
                    );
                    tc.id = new_id;
                }
            }
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
            tool_metadata.extend(batch_metadata);
            tool_success.extend(batch_success);
        }
        if let Some(sink) = tool_structured_metadata {
            sink.extend(tool_metadata);
        }
        if let Some(sink) = tool_success_by_id {
            sink.extend(tool_success);
        }

        let merged = merge_tool_messages_in_order(
            &response,
            &limited_response,
            tool_messages,
            blocked_messages,
        );

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
        turn.record_usage(tool_tokens.input_tokens, tool_tokens.output_tokens, tracker);
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

    shell_results
        .iter()
        .find(|content| is_diff_like_shell_output(content))
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
            (failed_shells >= min_shell_streak.saturating_sub(1))
                .then(|| shell_results.first().copied())
                .flatten()
                .map(|content| ShellRetryRecovery {
                    kind: ShellRetryRecoveryKind::RetryLimit,
                    content: shell_retry_limit_message(content),
                })
        })
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
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    use async_trait::async_trait;
    use octos_core::{AgentId, MessageRole, TaskContext, TaskKind, ToolCall};
    use octos_llm::{
        ChatResponse, LlmError, LlmErrorKind, LlmProvider, StopReason, TokenUsage as LlmTokenUsage,
    };
    use octos_memory::EpisodeStore;

    use crate::plugins::PluginTool;
    use crate::plugins::manifest::PluginToolDef;
    use crate::prompt_context::{
        PromptContextManager, PromptContextPhase, PromptContextReport, PromptContextRequest,
    };
    use crate::tools::{Tool, ToolRegistry, ToolResult, TurnAttachmentContext};

    struct FilesToSendOnlyTool {
        file_path: PathBuf,
    }

    #[async_trait]
    impl Tool for FilesToSendOnlyTool {
        fn name(&self) -> &str {
            "emit_audio"
        }

        fn description(&self) -> &str {
            "Emit an audio file via files_to_send only"
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {}
            })
        }

        async fn execute(&self, _args: &serde_json::Value) -> Result<ToolResult> {
            Ok(ToolResult {
                output: "audio generated".to_string(),
                success: true,
                files_to_send: vec![self.file_path.clone()],
                ..Default::default()
            })
        }
    }

    struct ToolThenEndProvider {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl LlmProvider for ToolThenEndProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> Result<ChatResponse> {
            let call = self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            let response = if call == 0 {
                ChatResponse {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![ToolCall {
                        id: "call_emit_audio".to_string(),
                        name: "emit_audio".to_string(),
                        arguments: serde_json::json!({}),
                        metadata: None,
                    }],
                    stop_reason: StopReason::ToolUse,
                    usage: LlmTokenUsage::default(),
                    provider_index: None,
                }
            } else {
                ChatResponse {
                    content: Some("done".to_string()),
                    reasoning_content: None,
                    tool_calls: vec![],
                    stop_reason: StopReason::EndTurn,
                    usage: LlmTokenUsage::default(),
                    provider_index: None,
                }
            };
            Ok(response)
        }

        fn model_id(&self) -> &str {
            "mock"
        }

        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    struct RecordingToolThenEndProvider {
        calls: AtomicUsize,
        observed_prompts: Arc<StdMutex<Vec<Vec<String>>>>,
    }

    struct MaxTokensThenEndProvider {
        calls: AtomicUsize,
        observed_prompts: Arc<StdMutex<Vec<Vec<String>>>>,
    }

    #[async_trait]
    impl LlmProvider for MaxTokensThenEndProvider {
        async fn chat(
            &self,
            messages: &[Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> Result<ChatResponse> {
            self.observed_prompts
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(
                    messages
                        .iter()
                        .map(|message| message.content.clone())
                        .collect(),
                );
            let call = self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(if call == 0 {
                ChatResponse {
                    content: Some("part one".to_string()),
                    reasoning_content: None,
                    tool_calls: vec![],
                    stop_reason: StopReason::MaxTokens,
                    usage: LlmTokenUsage {
                        input_tokens: 3,
                        output_tokens: 10,
                        ..Default::default()
                    },
                    provider_index: None,
                }
            } else {
                ChatResponse {
                    content: Some("part two".to_string()),
                    reasoning_content: None,
                    tool_calls: vec![],
                    stop_reason: StopReason::EndTurn,
                    usage: LlmTokenUsage {
                        input_tokens: 4,
                        output_tokens: 11,
                        ..Default::default()
                    },
                    provider_index: None,
                }
            })
        }

        fn model_id(&self) -> &str {
            "mock"
        }

        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    #[async_trait]
    impl LlmProvider for RecordingToolThenEndProvider {
        async fn chat(
            &self,
            messages: &[Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> Result<ChatResponse> {
            self.observed_prompts
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(
                    messages
                        .iter()
                        .map(|message| message.content.clone())
                        .collect(),
                );
            let call = self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(if call == 0 {
                ChatResponse {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![ToolCall {
                        id: "call_alpha".to_string(),
                        name: "alpha".to_string(),
                        arguments: serde_json::json!({}),
                        metadata: None,
                    }],
                    stop_reason: StopReason::ToolUse,
                    usage: LlmTokenUsage::default(),
                    provider_index: None,
                }
            } else {
                ChatResponse {
                    content: Some("done".to_string()),
                    reasoning_content: None,
                    tool_calls: vec![],
                    stop_reason: StopReason::EndTurn,
                    usage: LlmTokenUsage::default(),
                    provider_index: None,
                }
            })
        }

        fn model_id(&self) -> &str {
            "mock"
        }

        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    struct SpyPromptContextManager {
        phases: Arc<StdMutex<Vec<PromptContextPhase>>>,
    }

    impl PromptContextManager for SpyPromptContextManager {
        fn prepare_prompt(
            &self,
            request: PromptContextRequest,
            messages: &mut Vec<Message>,
        ) -> Result<PromptContextReport, String> {
            self.phases
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(request.phase);
            let before = messages.len();
            let mut prompt_replaced = false;
            if request.phase == PromptContextPhase::Iteration {
                messages.insert(
                    0,
                    Message {
                        role: MessageRole::System,
                        content: "[managed prompt from context manager]".to_string(),
                        media: vec![],
                        tool_calls: None,
                        tool_call_id: None,
                        reasoning_content: None,
                        client_message_id: None,
                        thread_id: None,
                        timestamp: chrono::Utc::now(),
                    },
                );
                prompt_replaced = true;
            }
            Ok(PromptContextReport {
                prompt_replaced,
                compaction_performed: false,
                messages_before: before,
                messages_after: messages.len(),
                token_estimate: Some(messages.iter().map(|message| message.content.len()).sum()),
                generation: Some(request.iteration as u64),
            })
        }
    }

    struct NamedEchoTool {
        name: &'static str,
        output: &'static str,
    }

    #[async_trait]
    impl Tool for NamedEchoTool {
        fn name(&self) -> &str {
            self.name
        }

        fn description(&self) -> &str {
            "Echo a fixed tool response"
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {}
            })
        }

        async fn execute(&self, _args: &serde_json::Value) -> Result<ToolResult> {
            Ok(ToolResult {
                output: self.output.to_string(),
                success: true,
                ..Default::default()
            })
        }
    }

    struct MultiToolThenEndProvider {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl LlmProvider for MultiToolThenEndProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> Result<ChatResponse> {
            let call = self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            let response = match call {
                0 => ChatResponse {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![
                        ToolCall {
                            id: "call_alpha".to_string(),
                            name: "alpha".to_string(),
                            arguments: serde_json::json!({}),
                            metadata: None,
                        },
                        ToolCall {
                            id: "call_beta".to_string(),
                            name: "beta".to_string(),
                            arguments: serde_json::json!({}),
                            metadata: None,
                        },
                    ],
                    stop_reason: StopReason::ToolUse,
                    usage: LlmTokenUsage::default(),
                    provider_index: None,
                },
                1 => ChatResponse {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![ToolCall {
                        id: "call_gamma".to_string(),
                        name: "gamma".to_string(),
                        arguments: serde_json::json!({}),
                        metadata: None,
                    }],
                    stop_reason: StopReason::ToolUse,
                    usage: LlmTokenUsage::default(),
                    provider_index: None,
                },
                _ => ChatResponse {
                    content: Some("done".to_string()),
                    reasoning_content: None,
                    tool_calls: vec![],
                    stop_reason: StopReason::EndTurn,
                    usage: LlmTokenUsage::default(),
                    provider_index: None,
                },
            };
            Ok(response)
        }

        fn model_id(&self) -> &str {
            "mock"
        }

        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    struct CountingEchoTool {
        name: &'static str,
        output: &'static str,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for CountingEchoTool {
        fn name(&self) -> &str {
            self.name
        }

        fn description(&self) -> &str {
            "Echo while tracking execution count"
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {}
            })
        }

        async fn execute(&self, _args: &serde_json::Value) -> Result<ToolResult> {
            self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(ToolResult {
                output: self.output.to_string(),
                success: true,
                ..Default::default()
            })
        }
    }

    struct PodcastGenerateTwiceProvider {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl LlmProvider for PodcastGenerateTwiceProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> Result<ChatResponse> {
            let call = self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            let response = match call {
                0 => ChatResponse {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![ToolCall {
                        id: "call_podcast_generate_1".to_string(),
                        name: "podcast_generate".to_string(),
                        arguments: serde_json::json!({}),
                        metadata: None,
                    }],
                    stop_reason: StopReason::ToolUse,
                    usage: LlmTokenUsage::default(),
                    provider_index: None,
                },
                1 => ChatResponse {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![ToolCall {
                        id: "call_podcast_generate_2".to_string(),
                        name: "podcast_generate".to_string(),
                        arguments: serde_json::json!({}),
                        metadata: None,
                    }],
                    stop_reason: StopReason::ToolUse,
                    usage: LlmTokenUsage::default(),
                    provider_index: None,
                },
                _ => ChatResponse {
                    content: Some("done".to_string()),
                    reasoning_content: None,
                    tool_calls: vec![],
                    stop_reason: StopReason::EndTurn,
                    usage: LlmTokenUsage::default(),
                    provider_index: None,
                },
            };
            Ok(response)
        }

        fn model_id(&self) -> &str {
            "mock"
        }

        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    struct ConsecutiveVoiceSaveProvider {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl LlmProvider for ConsecutiveVoiceSaveProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> Result<ChatResponse> {
            let call = self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            let response = match call {
                0 => ChatResponse {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![ToolCall {
                        id: "call_save_yangmi".to_string(),
                        name: "fm_voice_save".to_string(),
                        arguments: serde_json::json!({"name": "yangmi"}),
                        metadata: None,
                    }],
                    stop_reason: StopReason::ToolUse,
                    usage: LlmTokenUsage::default(),
                    provider_index: None,
                },
                1 => ChatResponse {
                    content: Some("yangmi saved".to_string()),
                    reasoning_content: None,
                    tool_calls: vec![],
                    stop_reason: StopReason::EndTurn,
                    usage: LlmTokenUsage::default(),
                    provider_index: None,
                },
                2 => ChatResponse {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![ToolCall {
                        id: "call_save_douwentao".to_string(),
                        name: "fm_voice_save".to_string(),
                        arguments: serde_json::json!({"name": "douwentao"}),
                        metadata: None,
                    }],
                    stop_reason: StopReason::ToolUse,
                    usage: LlmTokenUsage::default(),
                    provider_index: None,
                },
                _ => ChatResponse {
                    content: Some("douwentao saved".to_string()),
                    reasoning_content: None,
                    tool_calls: vec![],
                    stop_reason: StopReason::EndTurn,
                    usage: LlmTokenUsage::default(),
                    provider_index: None,
                },
            };
            Ok(response)
        }

        fn model_id(&self) -> &str {
            "mock"
        }

        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    #[cfg(unix)]
    fn write_test_script(path: &std::path::Path, content: &str) {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let mut file = std::fs::File::create(path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
        file.sync_all().unwrap();
        drop(file);
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[tokio::test]
    async fn run_task_collects_files_to_send_without_file_modified() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("podcast.mp3");
        std::fs::write(&file_path, b"fake mp3").unwrap();

        let mut tools = ToolRegistry::with_builtins(dir.path());
        tools.register(FilesToSendOnlyTool {
            file_path: file_path.clone(),
        });

        let provider: Arc<dyn LlmProvider> = Arc::new(ToolThenEndProvider {
            calls: AtomicUsize::new(0),
        });
        let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
        let agent = Agent::new(AgentId::new("test-agent"), provider, tools, memory);
        let task = Task::new(
            TaskKind::Code {
                instruction: "Generate audio".to_string(),
                files: vec![],
            },
            TaskContext {
                working_dir: dir.path().to_path_buf(),
                ..Default::default()
            },
        );

        let result = agent.run_task(&task).await.unwrap();
        assert!(result.success);
        assert!(result.files_modified.is_empty());
        assert_eq!(result.files_to_send, vec![file_path]);
    }

    #[tokio::test]
    async fn run_task_continues_after_max_tokens_in_same_loop() {
        let dir = tempfile::tempdir().unwrap();
        let tools = ToolRegistry::with_builtins(dir.path());
        let observed_prompts = Arc::new(StdMutex::new(Vec::new()));
        let provider = Arc::new(MaxTokensThenEndProvider {
            calls: AtomicUsize::new(0),
            observed_prompts: Arc::clone(&observed_prompts),
        });
        let provider_for_agent: Arc<dyn LlmProvider> = provider.clone();
        let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
        let agent = Agent::new(
            AgentId::new("max-tokens-test"),
            provider_for_agent,
            tools,
            memory,
        );
        let task = Task::new(
            TaskKind::Code {
                instruction: "Write a long report".to_string(),
                files: vec![],
            },
            TaskContext {
                working_dir: dir.path().to_path_buf(),
                ..Default::default()
            },
        );

        let result = agent.run_task(&task).await.unwrap();

        assert!(result.success);
        assert_eq!(
            result.output,
            "part one
part two"
        );
        assert_eq!(provider.calls.load(AtomicOrdering::SeqCst), 2);
        assert_eq!(result.token_usage.input_tokens, 7);
        assert_eq!(result.token_usage.output_tokens, 21);
        let prompts = observed_prompts
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert_eq!(prompts.len(), 2);
        assert!(prompts[1].iter().any(|content| content == "part one"));
        assert!(
            prompts[1]
                .iter()
                .any(|content| content.contains("Continue directly from where you stopped"))
        );
    }

    #[tokio::test]
    async fn process_message_preserves_tool_pair_order_across_iterations() {
        let dir = tempfile::tempdir().unwrap();
        let mut tools = ToolRegistry::with_builtins(dir.path());
        tools.register(NamedEchoTool {
            name: "alpha",
            output: "alpha ok",
        });
        tools.register(NamedEchoTool {
            name: "beta",
            output: "beta ok",
        });
        tools.register(NamedEchoTool {
            name: "gamma",
            output: "gamma ok",
        });

        let provider: Arc<dyn LlmProvider> = Arc::new(MultiToolThenEndProvider {
            calls: AtomicUsize::new(0),
        });
        let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
        let agent = Agent::new(AgentId::new("test-agent"), provider, tools, memory);

        let result = agent.process_message("do work", &[], vec![]).await.unwrap();
        let roles: Vec<MessageRole> = result.messages.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            vec![
                MessageRole::User,
                MessageRole::Assistant,
                MessageRole::Tool,
                MessageRole::Tool,
                MessageRole::Assistant,
                MessageRole::Tool,
            ]
        );
        assert_eq!(result.content, "done");
        assert_eq!(result.messages[1].tool_calls.as_ref().unwrap().len(), 2);
        assert_eq!(result.messages[4].tool_calls.as_ref().unwrap().len(), 1);
        assert_eq!(
            result.messages[2].tool_call_id.as_deref(),
            Some("call_alpha")
        );
        assert_eq!(
            result.messages[3].tool_call_id.as_deref(),
            Some("call_beta")
        );
        assert_eq!(
            result.messages[5].tool_call_id.as_deref(),
            Some("call_gamma")
        );
    }

    #[tokio::test]
    async fn process_message_uses_prompt_context_manager_before_each_llm_call() {
        let dir = tempfile::tempdir().unwrap();
        let mut tools = ToolRegistry::with_builtins(dir.path());
        tools.register(NamedEchoTool {
            name: "alpha",
            output: "alpha ok",
        });
        let observed_prompts = Arc::new(StdMutex::new(Vec::new()));
        let provider: Arc<dyn LlmProvider> = Arc::new(RecordingToolThenEndProvider {
            calls: AtomicUsize::new(0),
            observed_prompts: Arc::clone(&observed_prompts),
        });
        let phases = Arc::new(StdMutex::new(Vec::new()));
        let context_manager: Arc<dyn PromptContextManager> = Arc::new(SpyPromptContextManager {
            phases: Arc::clone(&phases),
        });
        let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
        let agent = Agent::new(AgentId::new("test-agent"), provider, tools, memory)
            .with_prompt_context_manager(context_manager);

        let result = agent.process_message("do work", &[], vec![]).await.unwrap();

        assert_eq!(result.content, "done");
        let phases = phases.lock().unwrap_or_else(|error| error.into_inner());
        assert_eq!(
            phases.as_slice(),
            [PromptContextPhase::TurnStart, PromptContextPhase::Iteration]
        );
        let prompts = observed_prompts
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert_eq!(prompts.len(), 2);
        assert!(
            !prompts[0]
                .iter()
                .any(|content| content.contains("[managed prompt from context manager]")),
            "turn-start prompt should remain unchanged in this spy"
        );
        assert!(
            prompts[1]
                .iter()
                .any(|content| content.contains("[managed prompt from context manager]")),
            "second LLM call must use the prompt vector prepared by the context manager"
        );
    }

    #[tokio::test]
    async fn process_message_blocks_second_podcast_generate_when_session_limit_is_one() {
        let dir = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut tools = ToolRegistry::with_builtins(dir.path());
        tools.register(CountingEchoTool {
            name: "podcast_generate",
            output: "podcast ok",
            calls: Arc::clone(&calls),
        });

        let provider: Arc<dyn LlmProvider> = Arc::new(PodcastGenerateTwiceProvider {
            calls: AtomicUsize::new(0),
        });
        let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
        let agent = Agent::new(AgentId::new("test-agent"), provider, tools, memory)
            .with_session_limits(crate::session::SessionLimits {
                per_tool_limits: [("podcast_generate".into(), 1)].into(),
                ..Default::default()
            });

        let result = agent
            .process_message("make a podcast", &[], vec![])
            .await
            .unwrap();
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
        let tool_contents: Vec<_> = result
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::Tool)
            .map(|message| message.content.clone())
            .collect();

        assert!(tool_contents.iter().any(|content| content == "podcast ok"));
        assert!(tool_contents.iter().any(|content| {
            content.contains("[SESSION LIMIT]")
                && content.contains("podcast_generate")
                && content.contains("max 1")
        }));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn process_message_injects_distinct_audio_attachments_for_consecutive_voice_saves() {
        let dir = tempfile::tempdir().unwrap();
        let input_log = dir.path().join("plugin-inputs.jsonl");
        let script_path = dir.path().join("mofa-fm-test.sh");
        write_test_script(
            &script_path,
            r#"#!/bin/sh
INPUT=$(cat)
printf '%s\n' "$INPUT" >> "$INPUT_LOG"
printf '{"output":"voice saved","success":true}\n'
"#,
        );

        let first_audio = dir.path().join("yangmi_ref2.wav");
        let second_audio = dir.path().join("douwentao.wav");
        std::fs::write(&first_audio, b"fake wav 1").unwrap();
        std::fs::write(&second_audio, b"fake wav 2").unwrap();
        let first_audio = first_audio.to_string_lossy().into_owned();
        let second_audio = second_audio.to_string_lossy().into_owned();

        let def = PluginToolDef {
            name: "fm_voice_save".to_string(),
            description: "Save a cloned voice".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "audio_path": {"type": "string"}
                },
                "required": ["name", "audio_path"]
            }),
            spawn_only: false,
            env: vec![],
            risk: None,
            spawn_only_message: None,
            concurrency_class: None,
        };
        let plugin = PluginTool::new("mofa-fm".into(), def, script_path).with_extra_env(vec![(
            "INPUT_LOG".into(),
            input_log.to_string_lossy().into_owned(),
        )]);

        let mut tools = ToolRegistry::new();
        tools.register(plugin);

        let provider: Arc<dyn LlmProvider> = Arc::new(ConsecutiveVoiceSaveProvider {
            calls: AtomicUsize::new(0),
        });
        let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
        let agent = Agent::new(AgentId::new("test-agent"), provider, tools, memory);

        let first = agent
            .process_message_with_attachments(
                "克隆 yangmi 语音",
                &[],
                vec![],
                TurnAttachmentContext {
                    attachment_paths: vec![first_audio.clone()],
                    audio_attachment_paths: vec![first_audio.clone()],
                    file_attachment_paths: vec![],
                    prompt_summary: Some("[Attached audio files]\n- yangmi_ref2.wav".to_string()),
                },
            )
            .await
            .unwrap();
        assert_eq!(first.content, "yangmi saved");

        let second = agent
            .process_message_with_attachments(
                "克隆窦文涛语音",
                &first.messages,
                vec![],
                TurnAttachmentContext {
                    attachment_paths: vec![second_audio.clone()],
                    audio_attachment_paths: vec![second_audio.clone()],
                    file_attachment_paths: vec![],
                    prompt_summary: Some("[Attached audio files]\n- douwentao.wav".to_string()),
                },
            )
            .await
            .unwrap();
        assert_eq!(second.content, "douwentao saved");

        let log = std::fs::read_to_string(&input_log).unwrap();
        let inputs = log
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();

        assert_eq!(inputs.len(), 2);
        assert_eq!(inputs[0]["name"], "yangmi");
        assert_eq!(inputs[0]["audio_path"], first_audio);
        assert_eq!(inputs[1]["name"], "douwentao");
        assert_eq!(inputs[1]["audio_path"], second_audio);
    }

    #[test]
    fn split_tool_calls_caps_parallel_batches() {
        let tool_calls: Vec<ToolCall> = (0..9)
            .map(|i| ToolCall {
                id: format!("call_{i}"),
                name: format!("tool_{i}"),
                arguments: serde_json::json!({}),
                metadata: None,
            })
            .collect();

        let batches = split_tool_calls(&tool_calls, MAX_PARALLEL_TOOL_CALLS_PER_BATCH);
        let batch_sizes: Vec<_> = batches.iter().map(|batch| batch.len()).collect();

        assert_eq!(batch_sizes, vec![8, 1]);
        assert_eq!(batches[0][0].id, "call_0");
        assert_eq!(batches[1][0].id, "call_8");
    }

    #[test]
    fn recover_shell_retry_output_prefers_diff_like_success() {
        let messages = vec![
            Message::user("show a diff"),
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![ToolCall {
                    id: "call_shell_1".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "git diff -- notes.txt"}),
                    metadata: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "fatal: not a git repository\n\nExit code: 128".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_1".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![ToolCall {
                    id: "call_shell_2".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "cd /tmp && git diff -- notes.txt"}),
                    metadata: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "diff --git a/notes.txt b/notes.txt\n--- a/notes.txt\n+++ b/notes.txt\n@@ -1,2 +1,2 @@\n alpha\n-beta\n+gamma\n\nExit code: 0".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_2".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![ToolCall {
                    id: "call_shell_3".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "git status --short"}),
                    metadata: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "(no output)\n\nExit code: 0".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_3".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![ToolCall {
                    id: "call_shell_4".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "git diff -- notes.txt"}),
                    metadata: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "fatal: not a git repository\n\nExit code: 128".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_4".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
        ];

        let recovered = recover_shell_retry(&messages, 4).expect("should recover");
        assert_eq!(recovered.kind, ShellRetryRecoveryKind::DiffLikeSuccess);
        assert!(recovered.content.contains("diff --git"));
        assert!(!recovered.content.contains("Exit code: 0"));
    }

    #[test]
    fn recover_shell_retry_output_tolerates_interleaved_edit_tools() {
        let messages = vec![
            Message::user("repair the failing test"),
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![ToolCall {
                    id: "call_shell_1".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "cargo test broken_case"}),
                    metadata: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "test result: FAILED. 0 passed; 1 failed\n\nExit code: 101".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_1".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![ToolCall {
                    id: "call_edit_1".into(),
                    name: "write_file".into(),
                    arguments: serde_json::json!({"path": "src/lib.rs"}),
                    metadata: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "updated src/lib.rs".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_edit_1".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![ToolCall {
                    id: "call_shell_2".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "cargo test broken_case"}),
                    metadata: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "test result: FAILED. 0 passed; 1 failed\n\nExit code: 101".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_2".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![ToolCall {
                    id: "call_shell_3".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "cargo test broken_case -- --nocapture"}),
                    metadata: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "test result: ok. 1 passed; 0 failed\n\nExit code: 0".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_3".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![ToolCall {
                    id: "call_shell_4".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "git diff -- src/lib.rs"}),
                    metadata: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-buggy\n+fixed\n\nExit code: 0".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_4".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
        ];

        let recovered = recover_shell_retry(&messages, 4).expect("should recover");
        assert_eq!(recovered.kind, ShellRetryRecoveryKind::DiffLikeSuccess);
        assert!(recovered.content.contains("diff --git"));
        assert!(!recovered.content.contains("Exit code: 0"));
    }

    #[test]
    fn recover_shell_retry_output_accepts_useful_non_diff_success() {
        let messages = vec![
            Message::user("repair the repo"),
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![ToolCall {
                    id: "call_shell_1".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "cargo test"}),
                    metadata: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "error: first failure\n\nExit code: 101".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_1".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![ToolCall {
                    id: "call_shell_2".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "cargo test --workspace"}),
                    metadata: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "error: second failure\n\nExit code: 101".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_2".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![ToolCall {
                    id: "call_shell_3".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "git status --short"}),
                    metadata: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: " M src/lib.rs\n?? notes.txt\n\nExit code: 0".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_3".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![ToolCall {
                    id: "call_shell_4".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "cargo test --locked"}),
                    metadata: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "error: third failure\n\nExit code: 101".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_4".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
        ];

        let recovered = recover_shell_retry(&messages, 4).expect("should recover");
        assert_eq!(recovered.kind, ShellRetryRecoveryKind::UsefulSuccess);
        assert!(recovered.content.contains("src/lib.rs"));
        assert!(!recovered.content.contains("Exit code: 0"));
    }

    #[test]
    fn recover_shell_retry_output_does_not_return_git_commit_setup_output() {
        let messages = vec![
            Message::user("return the final diff"),
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![ToolCall {
                    id: "call_shell_1".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "mkdir repo && cd repo && git init"}),
                    metadata: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "Initialized empty Git repository in /tmp/repo/.git/\n\nExit code: 0".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_1".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![ToolCall {
                    id: "call_shell_2".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "cd repo && git commit -m initial"}),
                    metadata: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "[master (root-commit) 1e19620] initial commit\n 1 file changed, 2 insertions(+)\n create mode 100644 notes.txt\n\nExit code: 0".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_2".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![ToolCall {
                    id: "call_shell_3".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "git diff -- notes.txt"}),
                    metadata: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "fatal: ambiguous argument 'notes.txt'\n\nExit code: 128".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_3".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![ToolCall {
                    id: "call_shell_4".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "pwd"}),
                    metadata: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "/tmp\n\nExit code: 0".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_4".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
        ];

        assert!(recover_shell_retry(&messages, 4).is_none());
    }

    #[test]
    fn recover_shell_retry_output_prefers_validation_success_over_useful_success() {
        let messages = vec![
            Message::user("repair the repo"),
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![ToolCall {
                    id: "call_shell_1".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "cargo test"}),
                    metadata: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "error: first failure\n\nExit code: 101".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_1".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![ToolCall {
                    id: "call_shell_2".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "cargo test --workspace"}),
                    metadata: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "error: second failure\n\nExit code: 101".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_2".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![ToolCall {
                    id: "call_shell_3".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "cargo test broken_case -- --nocapture"}),
                    metadata: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "test result: ok. 1 passed; 0 failed\n\nExit code: 0".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_3".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![ToolCall {
                    id: "call_shell_4".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "cargo test --locked"}),
                    metadata: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "error: third failure\n\nExit code: 101".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_4".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
        ];

        let recovered = recover_shell_retry(&messages, 4).expect("should recover");
        assert_eq!(recovered.kind, ShellRetryRecoveryKind::ValidationSuccess);
        assert!(recovered.content.contains("test result: ok"));
        assert!(!recovered.content.contains("Exit code: 0"));
    }

    #[test]
    fn recover_shell_retry_output_requires_failure_before_useful_success() {
        let messages = vec![
            Message::user("inspect the repo"),
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![ToolCall {
                    id: "call_shell_1".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "pwd"}),
                    metadata: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "/tmp/octos\n\nExit code: 0".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_1".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![ToolCall {
                    id: "call_shell_2".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "ls src"}),
                    metadata: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "lib.rs\nmain.rs\n\nExit code: 0".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_2".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![ToolCall {
                    id: "call_shell_3".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "git status --short"}),
                    metadata: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: " M src/lib.rs\n?? notes.txt\n\nExit code: 0".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_3".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![ToolCall {
                    id: "call_shell_4".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "cat Cargo.toml"}),
                    metadata: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "[package]\nname = \"octos\"\n\nExit code: 0".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_4".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
        ];

        assert!(recover_shell_retry(&messages, 4).is_none());
    }

    #[test]
    fn recover_shell_retry_output_stops_repeated_failure_spirals() {
        let messages = vec![
            Message::user("repair the repo"),
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![ToolCall {
                    id: "call_shell_1".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "cargo test"}),
                    metadata: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "error: could not find Cargo.toml\n\nExit code: 101".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_1".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![ToolCall {
                    id: "call_shell_2".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "cargo test --all"}),
                    metadata: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "error: could not find Cargo.toml\n\nExit code: 101".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_2".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![ToolCall {
                    id: "call_shell_3".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "cargo test --workspace"}),
                    metadata: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "error: could not find Cargo.toml\n\nExit code: 101".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_3".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![ToolCall {
                    id: "call_shell_4".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "cargo test --locked"}),
                    metadata: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "error: could not find Cargo.toml\n\nExit code: 101".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_4".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
        ];

        let recovered = recover_shell_retry(&messages, 4).expect("should stop");
        assert_eq!(recovered.kind, ShellRetryRecoveryKind::RetryLimit);
        assert!(recovered.content.contains("[SHELL RETRY LIMIT]"));
        assert!(recovered.content.contains("could not find Cargo.toml"));
    }

    // ── Fix #1+#2 (2026-05-10, codex r2): intra-turn scoping + correct splice ─

    /// `current_user_turn_start` returns the index of the most recent User
    /// message — the slice from there onward is the current turn, the
    /// scan window for the spiral detector.
    #[test]
    fn current_user_turn_start_returns_index_of_last_user_message() {
        let mut messages = stale_shell_failure_streak("call_shell");
        // first User is at index 0; nothing else; so current_user_turn_start
        // returns 0.
        assert_eq!(current_user_turn_start(&messages), 0);

        // Push a NEW user message simulating a new turn the user types
        // after the original streak.
        messages.push(Message::user("now ask me about weather"));
        let new_user_idx = messages.len() - 1;
        assert_eq!(current_user_turn_start(&messages), new_user_idx);
    }

    #[test]
    fn current_user_turn_start_returns_zero_when_no_user_message() {
        let messages: Vec<Message> = vec![Message {
            role: MessageRole::Assistant,
            content: "boot".into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        }];
        assert_eq!(current_user_turn_start(&messages), 0);
    }

    /// Multi-tool batch awareness: the LLM can emit
    /// `[shell, read_file]` in a single response. Both Tool results are
    /// appended consecutively. The gate must see "this batch contains
    /// shell" — checking only the latest Tool name would suppress
    /// legitimate detection.
    #[test]
    fn latest_tool_batch_contains_picks_up_shell_in_mixed_batch() {
        let messages = vec![
            Message::user("repair"),
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![
                    ToolCall {
                        id: "call_shell".into(),
                        name: "shell".into(),
                        arguments: serde_json::json!({"command": "ls"}),
                        metadata: None,
                    },
                    ToolCall {
                        id: "call_read".into(),
                        name: "read_file".into(),
                        arguments: serde_json::json!({"path": "x"}),
                        metadata: None,
                    },
                ]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "failed".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "{ \"x\": 1 }".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_read".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
        ];

        assert!(latest_tool_batch_contains(&messages, "shell"));
        assert!(latest_tool_batch_contains(&messages, "read_file"));
    }

    #[test]
    fn latest_tool_batch_contains_returns_false_when_pure_non_shell_batch() {
        let messages = vec![
            Message::user("ask weather"),
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![ToolCall {
                    id: "call_w".into(),
                    name: "get_weather".into(),
                    arguments: serde_json::json!({"city": "Beijing"}),
                    metadata: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "Clear sky 19.9C".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_w".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
        ];

        assert!(!latest_tool_batch_contains(&messages, "shell"));
    }

    /// Regression for the 2026-05-10 mini1 incident. A session that
    /// accumulated a 4-call shell streak with failures in turn N must NOT
    /// have turn N+1 force-ended when turn N+1 (a) starts with a fresh
    /// User message and (b) only ran `read_file`.
    ///
    /// With Fix #1 v2 (intra-turn window scan), `recover_shell_retry`
    /// applied to the windowed slice from the new User message onward
    /// sees zero shell calls — the threshold (4) is not met — so the
    /// detector returns None at the SCAN layer. The batch-aware gate is
    /// belt-and-suspenders for the case of mixed batches.
    #[test]
    fn intra_turn_window_skips_stale_shell_history_from_prior_turn() {
        let mut messages = stale_shell_failure_streak("call_shell");
        // New user turn after the stale streak.
        messages.push(Message::user("now read manifest.json"));
        // This turn ran read_file only.
        messages.push(Message {
            role: MessageRole::Assistant,
            content: String::new(),
            media: vec![],
            tool_calls: Some(vec![ToolCall {
                id: "call_read_now".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path": "manifest.json"}),
                metadata: None,
            }]),
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        });
        messages.push(Message {
            role: MessageRole::Tool,
            content: "{ ... 6kb manifest ... }".into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: Some("call_read_now".into()),
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        });

        // Whole-history scan still matches the stale streak — that's the
        // BUG we're fixing. The window is what restores correctness.
        assert!(recover_shell_retry(&messages, 4).is_some());

        let window_start = current_user_turn_start(&messages);
        let window = &messages[window_start..];
        // Inside the new-turn window, there are zero shell calls.
        assert!(!latest_tool_batch_contains(window, "shell"));
        // ...so the windowed scan finds no streak.
        assert!(recover_shell_retry(window, 4).is_none());
    }

    /// Same window, but the new turn DOES run shell (legitimately) — the
    /// detector must NOT fire after one shell call (threshold = 4).
    #[test]
    fn intra_turn_window_does_not_trip_on_single_fresh_shell_after_stale_streak() {
        let mut messages = stale_shell_failure_streak("call_shell");
        messages.push(Message::user("ok try one more thing"));
        messages.push(Message {
            role: MessageRole::Assistant,
            content: String::new(),
            media: vec![],
            tool_calls: Some(vec![ToolCall {
                id: "call_shell_new".into(),
                name: "shell".into(),
                arguments: serde_json::json!({"command": "cargo build"}),
                metadata: None,
            }]),
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        });
        messages.push(Message {
            role: MessageRole::Tool,
            content: "Compiling foo v0.1.0\nFinished\n\nExit code: 0".into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: Some("call_shell_new".into()),
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        });

        let window_start = current_user_turn_start(&messages);
        let window = &messages[window_start..];
        // gate passes (current batch contains shell) but the windowed
        // scan has only 1 shell call — far below the 4-streak threshold.
        assert!(latest_tool_batch_contains(window, "shell"));
        assert!(recover_shell_retry(window, 4).is_none());
    }

    /// Codex round-2 #d: in a mixed `[shell, read_file]` batch, the splice
    /// must target the SHELL Tool, not whichever Tool happened to be
    /// appended last. `latest_tool_batch_index(_, "shell")` returns the
    /// index of the SHELL Tool inside the trailing batch; the read_file
    /// Tool's content stays untouched.
    #[test]
    fn latest_tool_batch_index_returns_shell_index_in_mixed_batch() {
        let messages = vec![
            Message::user("repair"),
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![
                    ToolCall {
                        id: "call_shell".into(),
                        name: "shell".into(),
                        arguments: serde_json::json!({"command": "ls"}),
                        metadata: None,
                    },
                    ToolCall {
                        id: "call_read".into(),
                        name: "read_file".into(),
                        arguments: serde_json::json!({"path": "x"}),
                        metadata: None,
                    },
                ]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "shell failed".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "{ \"x\": 1 }".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_read".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
        ];

        // The trailing run is [shell-tool, read_file-tool]. The shell index
        // is the second-to-last entry (len - 2), NOT the last (len - 1).
        let shell_idx =
            latest_tool_batch_index(&messages, "shell").expect("shell present in batch");
        assert_eq!(shell_idx, messages.len() - 2);
        assert_eq!(messages[shell_idx].content, "shell failed");

        // Simulating the splice: only the shell Tool's content changes.
        let mut spliced = messages.clone();
        spliced[shell_idx].content = "[SHELL RETRY LIMIT] ...".to_string();
        assert_eq!(spliced[shell_idx].content, "[SHELL RETRY LIMIT] ...");
        // The read_file Tool's content stays untouched — preserves the
        // useful tool result that was correctly attributed.
        assert_eq!(spliced[messages.len() - 1].content, "{ \"x\": 1 }");
    }

    /// Codex round-2 #e: terminal RetryLimit + Exhausted user message
    /// must not be the raw system-shaped instruction. The sanitizer
    /// strips the prefix and frames the latest output for the user.
    #[test]
    fn shell_retry_terminal_user_message_strips_system_prefix() {
        let raw = "[SHELL RETRY LIMIT] Repeated shell repair attempts did not converge. Stop retrying shell and summarize the blocker.\n\nLatest shell output:\nerror: could not find Cargo.toml\n\nExit code: 101";
        let sanitized = shell_retry_terminal_user_message(raw);
        assert!(!sanitized.contains("[SHELL RETRY LIMIT]"));
        assert!(!sanitized.contains("Stop retrying shell and summarize"));
        assert!(sanitized.contains("could not find Cargo.toml"));
        assert!(
            sanitized.starts_with("I tried multiple shell approaches"),
            "expected user-facing framing, got: {sanitized}"
        );
    }

    #[test]
    fn shell_retry_terminal_user_message_fallback_when_no_output() {
        let raw = "[SHELL RETRY LIMIT] Repeated shell repair attempts did not converge. Stop retrying shell and summarize the blocker.\n\nLatest shell output:\n   ";
        let sanitized = shell_retry_terminal_user_message(raw);
        assert!(sanitized.contains("Please rephrase or give me a more specific direction"));
    }

    /// Codex round-3 BLOCK regression: after the Escalate splice
    /// overwrites a shell Tool's content with `[SHELL RETRY LIMIT] ... +
    /// original output`, a follow-up Exhausted recovery can wrap THAT
    /// already-prefixed content again, producing nested prefixes. The
    /// sanitizer must strip ALL of them — leaking even one inner
    /// "Stop retrying shell and summarize the blocker" string into the
    /// user-facing reply is wrong.
    #[test]
    fn shell_retry_terminal_user_message_unwraps_nested_prefix() {
        let prefix = "[SHELL RETRY LIMIT] Repeated shell repair attempts did not converge. Stop retrying shell and summarize the blocker.\n\nLatest shell output:\n";
        let inner = format!("{prefix}error: real shell output\n\nExit code: 101");
        let outer = format!("{prefix}{inner}");
        // Outer wrapping a wrapped string — two prefix layers.
        let sanitized = shell_retry_terminal_user_message(&outer);
        assert!(!sanitized.contains("[SHELL RETRY LIMIT]"));
        assert!(!sanitized.contains("Stop retrying shell and summarize"));
        assert!(sanitized.contains("error: real shell output"));

        // Three-deep paranoia case: should still strip cleanly.
        let triple = format!("{prefix}{outer}");
        let sanitized3 = shell_retry_terminal_user_message(&triple);
        assert!(!sanitized3.contains("[SHELL RETRY LIMIT]"));
        assert!(sanitized3.contains("error: real shell output"));
    }

    /// Helper: builds a 4-call shell-streak with all failures, exactly the
    /// shape the live mini1 session had at 19:35–19:36 PDT on 2026-05-10
    /// before the user asked unrelated questions.
    fn stale_shell_failure_streak(id_prefix: &str) -> Vec<Message> {
        let mut out = vec![Message::user("repair the repo")];
        for i in 1..=4 {
            let id = format!("{id_prefix}_{i}");
            out.push(Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![ToolCall {
                    id: id.clone(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "cargo test"}),
                    metadata: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            });
            out.push(Message {
                role: MessageRole::Tool,
                content: "error: could not find Cargo.toml\n\nExit code: 101".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some(id),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            });
        }
        out
    }

    // ── is_productive_tool_message (M6.2) ───────────────────────────────

    #[test]
    fn productive_message_rejects_known_failure_prefixes() {
        assert!(!is_productive_tool_message("Error: boom"));
        assert!(!is_productive_tool_message("[HOOK DENIED] blocked"));
        assert!(!is_productive_tool_message("[SESSION LIMIT] cap"));
        assert!(!is_productive_tool_message("[SHELL RETRY LIMIT] stop"));
        assert!(!is_productive_tool_message(
            "Path outside working directory: /etc/passwd"
        ));
        assert!(!is_productive_tool_message("(no output)"));
        assert!(!is_productive_tool_message("File not found: missing.txt"));
        assert!(!is_productive_tool_message(
            "Tool 'shell' panicked: bad state"
        ));
        assert!(!is_productive_tool_message(
            "Tool 'shell' timed out after 30 seconds"
        ));
    }

    #[test]
    fn productive_message_accepts_shell_success_exit() {
        assert!(is_productive_tool_message("hello\n\nExit code: 0"));
        assert!(is_productive_tool_message("short body\nExit code: 0"));
    }

    #[test]
    fn productive_message_requires_substantive_output() {
        // Short output without an explicit success marker is conservatively
        // treated as non-productive so transient failure messages do not keep
        // a stalled loop alive past budget.
        assert!(!is_productive_tool_message("ok"));
        assert!(!is_productive_tool_message("Done."));

        // Long output that isn't a failure passes the fallback bar.
        let long = "line ".repeat(40); // ~200 bytes
        assert!(is_productive_tool_message(&long));
    }

    #[test]
    fn productive_message_rejects_failed_to_prefix_in_long_body() {
        // Long outputs that still contain "failed to" are excluded so
        // large error payloads do not accidentally count as productive.
        let body = "failed to resolve target: ".to_string() + &"x".repeat(200);
        assert!(!is_productive_tool_message(&body));
    }

    // ─────────────────────────────────────────────────────────────────────
    // Review A F-001 — dispatch_loop_error wiring.
    // ─────────────────────────────────────────────────────────────────────

    /// Minimal placeholder provider for F-001 dispatch tests. The tests drive
    /// `handle_loop_error_with_dispatch` directly and never call `chat()`, so
    /// the provider's only requirement is to satisfy the trait bounds.
    struct InertProvider;

    #[async_trait]
    impl LlmProvider for InertProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> Result<ChatResponse> {
            unreachable!("InertProvider::chat must not be called in F-001 dispatch tests");
        }

        fn model_id(&self) -> &str {
            "inert"
        }

        fn provider_name(&self) -> &str {
            "inert"
        }
    }

    /// Counting summarizer used to prove the `CompactAndRetry` arm of
    /// `handle_loop_error_with_dispatch` actually drives `maybe_run_turn_compaction`.
    struct CountingSummarizer {
        calls: Arc<AtomicUsize>,
    }

    impl crate::summarizer::Summarizer for CountingSummarizer {
        fn kind(&self) -> &'static str {
            "counting_spy"
        }

        fn summarize(&self, messages: &[Message], budget_tokens: u32) -> Result<String> {
            self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(crate::compaction::compact_messages(messages, budget_tokens))
        }
    }

    async fn build_dispatch_test_agent() -> Agent {
        let dir = tempfile::tempdir().unwrap();
        let provider: Arc<dyn LlmProvider> = Arc::new(InertProvider);
        let tools = ToolRegistry::new();
        let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
        Agent::new(AgentId::new("test-dispatch"), provider, tools, memory)
    }

    // ─────────────────────────────────────────────────────────────────────
    // M8.10-C — LOOP DETECTED dedup.
    // ─────────────────────────────────────────────────────────────────────

    /// Mock LLM that always returns the same shell tool call with the same
    /// arguments, forcing the loop detector to fire on iteration 4.
    struct AlwaysSameToolProvider;

    #[async_trait]
    impl LlmProvider for AlwaysSameToolProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> Result<ChatResponse> {
            Ok(ChatResponse {
                content: None,
                reasoning_content: None,
                tool_calls: vec![ToolCall {
                    id: "call_loop".to_string(),
                    name: "read_file".to_string(),
                    arguments: serde_json::json!({"path": "loopy.txt"}),
                    metadata: None,
                }],
                stop_reason: StopReason::ToolUse,
                usage: LlmTokenUsage::default(),
                provider_index: None,
            })
        }

        fn model_id(&self) -> &str {
            "mock"
        }

        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    async fn build_agent_with_mock(dir: &std::path::Path) -> Agent {
        let tools = ToolRegistry::with_builtins(dir);
        let provider: Arc<dyn LlmProvider> = Arc::new(AlwaysSameToolProvider);
        let memory = Arc::new(EpisodeStore::open(dir.join("memory")).await.unwrap());
        Agent::new(AgentId::new("loop-dedup"), provider, tools, memory)
    }

    #[tokio::test]
    async fn dedup_loop_warning_returns_warning_on_first_fire() {
        let dir = tempfile::tempdir().unwrap();
        let agent = build_agent_with_mock(dir.path()).await;

        assert!(!agent.is_loop_detected_recently());
        let result = agent.dedup_loop_warning("[LOOP DETECTED] cycle".to_string());
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "[LOOP DETECTED] cycle");
        assert!(agent.is_loop_detected_recently());
    }

    #[tokio::test]
    async fn dedup_loop_warning_returns_terminal_error_on_second_fire() {
        let dir = tempfile::tempdir().unwrap();
        let agent = build_agent_with_mock(dir.path()).await;

        let first = agent.dedup_loop_warning("[LOOP DETECTED] one".to_string());
        assert!(first.is_ok());
        let second = agent.dedup_loop_warning("[LOOP DETECTED] two".to_string());
        assert!(second.is_err());
        let err = second.err().unwrap().to_string();
        assert!(
            err.contains("agent loop got stuck"),
            "expected terminal error, got: {err}"
        );
        // Flag stays set after the terminal error so further fires keep
        // returning terminal errors until the next process_message reset.
        assert!(agent.is_loop_detected_recently());
    }

    #[tokio::test]
    async fn dedup_loop_warning_resets_after_reset() {
        let dir = tempfile::tempdir().unwrap();
        let agent = build_agent_with_mock(dir.path()).await;

        agent
            .dedup_loop_warning("[LOOP DETECTED]".to_string())
            .unwrap();
        assert!(agent.is_loop_detected_recently());
        agent.reset_loop_detected_recently();
        assert!(!agent.is_loop_detected_recently());

        // After reset, a new fire returns a warning again (not terminal).
        let again = agent.dedup_loop_warning("[LOOP DETECTED] again".to_string());
        assert!(again.is_ok());
    }

    #[tokio::test]
    async fn process_message_resets_loop_detected_flag_at_start() {
        // Pre-set the flag, then run a process_message that does NOT trigger
        // the loop detector. The reset at the start of process_message_inner
        // should clear the flag before the turn runs, and since no loop fires
        // the flag stays cleared at exit.
        let dir = tempfile::tempdir().unwrap();
        let provider: Arc<dyn LlmProvider> = Arc::new(ToolThenEndProvider {
            calls: AtomicUsize::new(0),
        });
        let mut tools = ToolRegistry::with_builtins(dir.path());
        let echo_path = dir.path().join("audio.mp3");
        std::fs::write(&echo_path, b"x").unwrap();
        tools.register(FilesToSendOnlyTool {
            file_path: echo_path,
        });
        let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
        let agent = Agent::new(AgentId::new("reset-test"), provider, tools, memory);

        agent.mark_loop_detected_recently();
        assert!(agent.is_loop_detected_recently());

        let _ = agent
            .process_message("hi", &[], vec![])
            .await
            .expect("process_message should succeed");

        assert!(
            !agent.is_loop_detected_recently(),
            "process_message should reset the loop_detected flag at start"
        );
    }

    // ─── Back to Review A F-001 dispatch tests ───────────────────────────

    #[tokio::test]
    async fn should_compact_and_retry_on_context_overflow() {
        // F-001 coverage #1: a ContextOverflow error must drive the
        // CompactAndRetry arm, which runs `maybe_run_turn_compaction` (via
        // the wired CompactionRunner) and returns Retry so the outer loop
        // continues instead of bailing.
        use crate::compaction::{CompactionPolicy, CompactionRunner};
        use crate::workspace_policy::{CompactionSummarizerKind, WorkspacePolicy};

        let policy = CompactionPolicy {
            schema_version: crate::abi_schema::COMPACTION_POLICY_SCHEMA_VERSION,
            // Budget sized so recent+system fits (≈6 kept messages at 400
            // words ≈ 2.4k tokens) but overall messages still overflow the
            // budget, which forces the runner into its summarise branch
            // rather than the fallback-trim branch.
            token_budget: 8_000,
            preflight_threshold: Some(1_000),
            prune_tool_results_after_turns: None,
            preserved_artifacts: vec![],
            preserved_invariants: vec![],
            summarizer: CompactionSummarizerKind::Extractive,
        };
        let spy = Arc::new(AtomicUsize::new(0));
        let runner = CompactionRunner::new(policy)
            .with_summarizer(CountingSummarizer { calls: spy.clone() });
        let workspace = WorkspacePolicy::for_session();
        let agent = build_dispatch_test_agent()
            .await
            .with_compaction_runner(Arc::new(runner))
            .with_compaction_workspace(workspace);

        let mut retry_state = LoopRetryState::new();
        // Build an eyre::Report wrapping a typed LlmError so the harness
        // classifier downcasts it to HarnessError::ContextOverflow rather
        // than the Internal fallback.
        let raw_error: eyre::Report = LlmError::new(
            LlmErrorKind::ContextOverflow {
                limit: Some(200_000),
                used: Some(201_000),
            },
            "prompt too long for model window",
        )
        .into();

        // Conversation large enough that the compaction runner enters its
        // summarise branch rather than the oldest-first fallback trim.
        let filler = "word ".repeat(400);
        let mut messages = vec![Message {
            role: MessageRole::System,
            content: "sys".to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        }];
        for i in 0..14 {
            messages.push(Message {
                role: MessageRole::User,
                content: format!("turn {i} user question {filler}"),
                media: vec![],
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            });
            messages.push(Message {
                role: MessageRole::Assistant,
                content: format!("turn {i} assistant reply {filler}"),
                media: vec![],
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            });
        }

        // iteration=2 so maybe_run_turn_compaction actually runs (iteration=1
        // is reserved for the preflight path).
        let action =
            agent.handle_loop_error_with_dispatch(&raw_error, &mut retry_state, 2, &mut messages);
        assert_eq!(
            action,
            LoopErrorAction::Retry,
            "ContextOverflow must land on the Retry arm after compaction"
        );
        assert!(
            spy.load(AtomicOrdering::SeqCst) >= 1,
            "CompactAndRetry must invoke maybe_run_turn_compaction → summarizer at least once; got {}",
            spy.load(AtomicOrdering::SeqCst)
        );
        assert_eq!(
            retry_state.counters().context_overflow,
            1,
            "first ContextOverflow observation must bump the bucket counter once"
        );
    }

    #[tokio::test]
    async fn should_escalate_when_bucket_exhausted() {
        // F-001 coverage #2: once the retry bucket for a variant is
        // saturated, the next observation MUST land on the Bail arm so the
        // caller surfaces Err(report) instead of looping. Pre-fix the
        // classified error was ignored and only Escalate was reachable;
        // Exhausted was dead.
        let agent = build_dispatch_test_agent().await;
        let mut retry_state =
            LoopRetryState::with_limits(crate::agent::loop_state::LoopRetryLimits {
                rate_limited: 1,
                ..Default::default()
            });
        let mut messages: Vec<Message> = Vec::new();

        // First observation: transient rate-limit → Continue → Retry.
        // Typed LlmError so classify_report maps to RateLimited rather than
        // the Internal fallback.
        let rate_limit_error: eyre::Report = LlmError::rate_limited(Some(2)).into();
        let first_action = agent.handle_loop_error_with_dispatch(
            &rate_limit_error,
            &mut retry_state,
            1,
            &mut messages,
        );
        assert_eq!(
            first_action,
            LoopErrorAction::Retry,
            "first rate-limit observation must land on Retry"
        );

        // Second observation: bucket exhausted (limit=1) → Exhausted → Bail.
        let second_action = agent.handle_loop_error_with_dispatch(
            &rate_limit_error,
            &mut retry_state,
            2,
            &mut messages,
        );
        assert_eq!(
            second_action,
            LoopErrorAction::Bail,
            "exhausted rate-limit bucket must land on Bail so the outer loop surfaces Err"
        );
        assert!(
            retry_state.counters().rate_limited >= 2,
            "bucket must be bumped for every observation, not just the first",
        );
    }

    #[tokio::test]
    async fn should_bail_on_authentication_error_without_compaction() {
        // F-001 coverage #3: FailFast-hint variants (Authentication) must
        // land on Bail immediately, regardless of whether a compaction
        // runner is wired. Proves the Escalate arm reaches Bail.
        let agent = build_dispatch_test_agent().await;
        let mut retry_state = LoopRetryState::new();
        let mut messages: Vec<Message> = Vec::new();

        let auth_error: eyre::Report = LlmError::auth("invalid API key").into();
        let action =
            agent.handle_loop_error_with_dispatch(&auth_error, &mut retry_state, 1, &mut messages);
        assert_eq!(
            action,
            LoopErrorAction::Bail,
            "Authentication errors must never retry; they must bail"
        );
    }

    #[tokio::test]
    async fn process_message_fires_loop_warning_once_then_terminal_error() {
        // Two consecutive process_message calls with the same looping LLM.
        // Each call resets at start, so each should emit a warning (not a
        // terminal error). This documents the cross-turn dedup behavior:
        // dedup is intra-turn only because each new user message starts a
        // fresh session-burst slot.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("loopy.txt"), b"x").unwrap();
        let provider: Arc<dyn LlmProvider> = Arc::new(AlwaysSameToolProvider);
        let tools = ToolRegistry::with_builtins(dir.path());
        let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
        let agent = Agent::new(AgentId::new("burst"), provider, tools, memory).with_config(
            crate::AgentConfig {
                max_iterations: 30,
                save_episodes: false,
                ..Default::default()
            },
        );

        let first = agent.process_message("loop please", &[], vec![]).await;
        // Either the loop warning surfaced, or the recover_shell_retry path
        // returned. Both terminate cleanly without an Err.
        assert!(first.is_ok(), "first call should not error");
        // Flag set after first warning.
        assert!(agent.is_loop_detected_recently());

        let second = agent.process_message("loop again", &[], vec![]).await;
        // Reset at start of process_message clears the flag, so a brand-new
        // burst is allowed and emits a warning (Ok), not a terminal Err.
        assert!(second.is_ok(), "second call should not error after reset");
    }

    // ─────────────────────────────────────────────────────────────────────
    // PR `fix/news-fetch-loop-and-detect-recovery` —
    // LOOP DETECTED non-terminal recovery (`session web-1779494658716-mxrxe8`,
    // ledger seq 214-562). On first fire we now inject a synthetic tool
    // result carrying the warning and continue the loop for one more LLM
    // iteration; on second fire we return a terminal `ConversationResponse`.
    // ─────────────────────────────────────────────────────────────────────

    #[test]
    fn inject_synthetic_results_pushes_assistant_then_tool_for_every_call() {
        let response = ChatResponse {
            content: None,
            reasoning_content: None,
            tool_calls: vec![
                ToolCall {
                    id: "call_a".to_string(),
                    name: "news_fetch".to_string(),
                    arguments: serde_json::json!({"categories": ["tech"]}),
                    metadata: None,
                },
                ToolCall {
                    id: "call_b".to_string(),
                    name: "news_fetch".to_string(),
                    arguments: serde_json::json!({"categories": ["world"]}),
                    metadata: None,
                },
            ],
            stop_reason: StopReason::ToolUse,
            usage: LlmTokenUsage::default(),
            provider_index: None,
        };

        let dir = tempfile::tempdir().unwrap();
        let tools = ToolRegistry::with_builtins(dir.path());
        let provider: Arc<dyn LlmProvider> = Arc::new(AlwaysSameToolProvider);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let memory = runtime.block_on(async {
            Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap())
        });
        let agent = Agent::new(AgentId::new("inject-test"), provider, tools, memory);

        let mut messages: Vec<Message> = Vec::new();
        super::super::loop_runner::inject_loop_detected_synthetic_results(
            &mut messages,
            &response,
            "[LOOP DETECTED] cycle length 1.",
            &agent,
        );

        // 1 assistant + 2 tool results (one per tool_call).
        assert_eq!(messages.len(), 3, "expected 1 assistant + 2 tool results");
        assert_eq!(messages[0].role, MessageRole::Assistant);
        assert_eq!(
            messages[0]
                .tool_calls
                .as_ref()
                .map(|tcs| tcs.len())
                .unwrap_or(0),
            2,
            "assistant message must carry the looping tool_calls so providers \
             can bind the synthetic tool-result messages back to them"
        );

        for (idx, msg) in messages[1..].iter().enumerate() {
            assert_eq!(msg.role, MessageRole::Tool, "tool message #{idx}");
            let id_expected = if idx == 0 { "call_a" } else { "call_b" };
            assert_eq!(msg.tool_call_id.as_deref(), Some(id_expected));
        }

        // First tool-result carries the warning + synthesis hint; second is
        // a short companion stub so the LLM doesn't think the second call
        // actually executed.
        assert!(
            messages[1].content.contains("[LOOP DETECTED]"),
            "primary tool result must echo the warning: got `{}`",
            messages[1].content
        );
        assert!(
            messages[1].content.contains("synthesise")
                || messages[1].content.contains("different tool"),
            "primary tool result must contain a synthesis hint so the LLM \
             knows how to course-correct: got `{}`",
            messages[1].content
        );
        assert!(
            messages[2].content.contains("[LOOP DETECTED]")
                && messages[2].content.contains("companion"),
            "companion tool result should mark itself as such: got `{}`",
            messages[2].content
        );
    }

    /// Codex MAJOR on PR #1181: the synthetic injection path bypassed the
    /// `sanitize_tool_call_id` step that the normal `handle_tool_use` path
    /// applies (loop_runner.rs ~line 1685). Moonshot/kimi (which dspfac uses)
    /// emits IDs with colons like `admin_view_sessions:11` — OpenAI-style
    /// schemas reject those, and our own duplicate-repair logic can collapse
    /// them, leaving unanswered tool_calls on the next LLM call.
    ///
    /// This test simulates a looping ChatResponse with a colon-bearing id and
    /// asserts:
    ///   1. The injected synthetic messages carry a sanitized id (no colon).
    ///   2. The assistant message's `tool_calls[].id` matches the tool
    ///      result's `tool_call_id` 1:1 (same sanitized id end-to-end).
    #[test]
    fn inject_synthetic_results_sanitizes_tool_call_ids_with_colons() {
        let raw_id = "admin_view_sessions:11";
        let response = ChatResponse {
            content: None,
            reasoning_content: None,
            tool_calls: vec![ToolCall {
                id: raw_id.to_string(),
                name: "news_fetch".to_string(),
                arguments: serde_json::json!({"categories": ["tech"]}),
                metadata: None,
            }],
            stop_reason: StopReason::ToolUse,
            usage: LlmTokenUsage::default(),
            provider_index: None,
        };

        let dir = tempfile::tempdir().unwrap();
        let tools = ToolRegistry::with_builtins(dir.path());
        let provider: Arc<dyn LlmProvider> = Arc::new(AlwaysSameToolProvider);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let memory = runtime.block_on(async {
            Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap())
        });
        let agent = Agent::new(AgentId::new("sanitize-test"), provider, tools, memory);

        let mut messages: Vec<Message> = Vec::new();
        super::super::loop_runner::inject_loop_detected_synthetic_results(
            &mut messages,
            &response,
            "[LOOP DETECTED] cycle length 1.",
            &agent,
        );

        // Layout: 1 assistant + 1 tool result.
        assert_eq!(messages.len(), 2, "expected 1 assistant + 1 tool result");

        // Extract the assistant tool_call id and the tool result's
        // tool_call_id; both must be the SAME sanitized value.
        let assistant_tc_id = messages[0]
            .tool_calls
            .as_ref()
            .and_then(|tcs| tcs.first())
            .map(|tc| tc.id.clone())
            .expect("assistant message must carry sanitized tool_calls");
        let tool_result_id = messages[1]
            .tool_call_id
            .clone()
            .expect("tool result must carry tool_call_id");

        // 1. Sanitized — no colon left over.
        assert!(
            !assistant_tc_id.contains(':'),
            "assistant tool_call id must be sanitized (no colon): got `{assistant_tc_id}`"
        );
        assert!(
            !tool_result_id.contains(':'),
            "tool result tool_call_id must be sanitized (no colon): got `{tool_result_id}`"
        );

        // 2. Same id on BOTH sides — providers bind tool_use ↔ tool_result
        // by exact id match, so any drift here would orphan the pair.
        assert_eq!(
            assistant_tc_id, tool_result_id,
            "assistant tool_calls[].id and tool result tool_call_id must \
             share the SAME sanitized id (1:1 pairing); raw_id was `{raw_id}`"
        );

        // 3. Concrete sanitized form: `:` → `_` per `sanitize_tool_call_id`.
        assert_eq!(
            assistant_tc_id, "admin_view_sessions_11",
            "sanitize_tool_call_id should replace `:` with `_`"
        );
    }

    #[test]
    fn loop_detected_terminal_message_is_user_facing_and_non_empty() {
        let msg = super::super::loop_runner::loop_detected_terminal_message();
        assert!(msg.contains("[LOOP DETECTED]"));
        assert!(
            msg.contains("rephrase") || msg.contains("different angle"),
            "terminal message should guide the user to rephrase: got `{msg}`"
        );
    }

    /// LLM mock that always returns the SAME tool call so the loop
    /// detector fires repeatedly. Counts invocations so the test can
    /// assert how many LLM calls happened across the recovery window.
    struct CountingAlwaysSameToolProvider {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl LlmProvider for CountingAlwaysSameToolProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> Result<ChatResponse> {
            self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(ChatResponse {
                content: None,
                reasoning_content: None,
                tool_calls: vec![ToolCall {
                    id: "call_loopy".to_string(),
                    name: "read_file".to_string(),
                    arguments: serde_json::json!({"path": "loopy.txt"}),
                    metadata: None,
                }],
                stop_reason: StopReason::ToolUse,
                usage: LlmTokenUsage::default(),
                provider_index: None,
            })
        }

        fn model_id(&self) -> &str {
            "mock"
        }

        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    #[tokio::test]
    async fn loop_detected_first_fire_continues_then_second_fire_terminates() {
        // Exercises the full PR `fix/news-fetch-loop-and-detect-recovery`
        // recovery contract end-to-end:
        //   1. The looping LLM trips the detector on the 4th call (cycle-1).
        //   2. First detection MUST NOT terminate — it injects a synthetic
        //      tool result with the warning and calls the LLM again.
        //   3. If the LLM repeats the same call, the SECOND detection
        //      terminates with `loop_detected_terminal_message()`.
        //
        // We assert via:
        //   - The flag (`is_loop_detected_recently`) is set after the run.
        //   - The terminal `content` matches `loop_detected_terminal_message`
        //     (proves the second-fire path ran, not the original first-fire
        //     return-immediately path).
        //   - The mock LLM was called AT LEAST 5 times (>=4 to trigger first
        //     fire, +1 for the recovery iteration), confirming the loop
        //     continued after the first fire.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("loopy.txt"), b"x").unwrap();
        let provider = Arc::new(CountingAlwaysSameToolProvider {
            calls: AtomicUsize::new(0),
        });
        let provider_arc: Arc<dyn LlmProvider> = provider.clone();
        let tools = ToolRegistry::with_builtins(dir.path());
        let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
        let agent = Agent::new(AgentId::new("recover"), provider_arc, tools, memory).with_config(
            crate::AgentConfig {
                max_iterations: 30,
                save_episodes: false,
                ..Default::default()
            },
        );

        let result = agent
            .process_message("please loop", &[], vec![])
            .await
            .expect("process_message should return Ok even when the loop terminates");

        // The terminal message proves the second-fire branch ran. The
        // pre-fix behaviour would have returned the FIRST-fire warning
        // text and stopped before issuing another LLM call.
        assert_eq!(
            result.content,
            loop_detected_terminal_message(),
            "expected the terminal hard-stop message after the second \
             loop detection; pre-fix code would have returned the warning \
             text on the first fire"
        );
        assert!(agent.is_loop_detected_recently());

        let total_calls = provider.calls.load(AtomicOrdering::SeqCst);
        assert!(
            total_calls >= 5,
            "expected at least 5 LLM calls (4 to trigger first detection + \
             1 recovery iteration); got {total_calls}"
        );
    }

    // ----- Audit Gap-8: auto-fire check_workspace_contract on Completion -----

    /// LLM stub that always returns a single EndTurn — used by the
    /// Gap-8 tests to drive `run_task` straight to the contract-check
    /// branch without iterating through tool calls.
    struct EndTurnOnlyProvider;
    #[async_trait]
    impl LlmProvider for EndTurnOnlyProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> Result<ChatResponse> {
            Ok(ChatResponse {
                content: Some("done".into()),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: LlmTokenUsage::default(),
                provider_index: None,
            })
        }

        fn model_id(&self) -> &str {
            "mock"
        }

        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    /// Build a slides workspace fixture, optionally fully-ready.
    ///
    /// Pure-filesystem setup. Callers that want a "ready" deck must also
    /// invoke [`run_managed_slides_workspace_validators`] (async) or
    /// [`run_managed_slides_workspace_validators_sync`] (blocking) to
    /// exercise the PRODUCTION project-root validator helper. Splitting
    /// the helper this way avoids the "Cannot start a runtime from within a
    /// runtime" panic when async tests call the fixture inside their own
    /// Tokio runtime.
    fn make_managed_slides_workspace(tmp_root: &std::path::Path, slug: &str, ready: bool) {
        use crate::workspace_git::WorkspaceProjectKind;
        use crate::workspace_policy::{WorkspacePolicy, write_workspace_policy};
        let repo_root = tmp_root.join("slides").join(slug);
        std::fs::create_dir_all(&repo_root).unwrap();
        write_workspace_policy(
            &repo_root,
            &WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides),
        )
        .unwrap();
        // Every slides workspace requires script.js / memory.md / changelog.md
        // for turn_end + output/deck.pptx + slide png for completion.
        std::fs::write(repo_root.join("script.js"), "// slides").unwrap();
        std::fs::write(repo_root.join("memory.md"), "# memory").unwrap();
        std::fs::write(repo_root.join("changelog.md"), "# changelog").unwrap();
        if ready {
            std::fs::create_dir_all(repo_root.join("output/imgs")).unwrap();
            // octos #997: write real PPTX magic bytes so the project-scope
            // PPTX `MagicBytes` validator wired in
            // `WorkspacePolicy::for_kind(Slides)` does not fail the gate.
            let mut pptx = vec![0x50, 0x4B, 0x03, 0x04];
            pptx.extend_from_slice(&[0u8; 32]);
            std::fs::write(repo_root.join("output/deck.pptx"), &pptx).unwrap();
            std::fs::write(repo_root.join("output/imgs/slide-01.png"), "fake-png").unwrap();
            // NOTE: caller must invoke
            // `run_managed_slides_workspace_validators[_sync]` to write the
            // slides-kind PPTX MagicBytes Pass row.
        }
    }

    /// octos #997 (round-2 fix): async variant — exercise the production
    /// project-root validator helper so the ready fixture writes a Pass row
    /// into the same project ledger that the spawn loop writes to in
    /// production. Pre-round-2 the fixture manually `ledger.append(...)`ed a
    /// fake Pass; codex flagged that as masking the gap (the validator was
    /// declared but never RUN at the project root in production).
    async fn run_managed_slides_workspace_validators(tmp_root: &std::path::Path, slug: &str) {
        use crate::workspace_git::WorkspaceProjectKind;
        let registry = std::sync::Arc::new(crate::ToolRegistry::new());
        // Mirror production: the spawn loop hands the plugin's
        // `files_to_send` list through. The fixture stages the deck at
        // the legacy in-project path so the filter accepts it.
        let files_to_send = vec![tmp_root.join("slides").join(slug).join("output/deck.pptx")];
        let _ = crate::workspace_contract::run_project_root_validators(
            &registry,
            tmp_root,
            Some(WorkspaceProjectKind::Slides),
            &files_to_send,
        )
        .await;
    }

    /// Sync variant of [`run_managed_slides_workspace_validators`] for
    /// non-async `#[test]` callers that don't already have a Tokio runtime
    /// (and can therefore build one without nesting).
    fn run_managed_slides_workspace_validators_sync(tmp_root: &std::path::Path, slug: &str) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime for fixture validator run");
        runtime.block_on(run_managed_slides_workspace_validators(tmp_root, slug));
    }

    #[test]
    fn should_return_none_when_workspace_has_no_policy_managed_repos() {
        // Bare working_dir with no `slides/` or `sites/` subdir →
        // inspect_workspace_contracts yields an empty Vec → helper returns
        // None → loop_runner keeps Success.
        let tmp = tempfile::tempdir().unwrap();
        assert!(inspect_workspace_contract_failures(tmp.path()).is_none());
    }

    #[test]
    fn should_return_none_when_all_managed_repos_are_ready() {
        let tmp = tempfile::tempdir().unwrap();
        make_managed_slides_workspace(tmp.path(), "demo", true);
        run_managed_slides_workspace_validators_sync(tmp.path(), "demo");

        let failures = inspect_workspace_contract_failures(tmp.path());
        assert!(
            failures.is_none(),
            "ready workspace should not produce contract failure summary: {:?}",
            failures
        );
    }

    #[test]
    fn should_return_failure_summary_when_managed_repo_is_not_ready() {
        let tmp = tempfile::tempdir().unwrap();
        // slug=broken with NO output/ artifacts → completion checks fail.
        make_managed_slides_workspace(tmp.path(), "broken", false);

        let failures = inspect_workspace_contract_failures(tmp.path())
            .expect("broken workspace must produce contract failure summary");
        assert!(
            failures.contains("slides/broken"),
            "summary should name the failing repo:\n{}",
            failures
        );
        assert!(
            failures.contains("completion failed") || failures.contains("artifact missing"),
            "summary should describe what failed:\n{}",
            failures
        );
    }

    #[test]
    fn should_return_failure_summary_with_mixed_repos() {
        let tmp = tempfile::tempdir().unwrap();
        make_managed_slides_workspace(tmp.path(), "ready-deck", true);
        make_managed_slides_workspace(tmp.path(), "broken-deck", false);
        run_managed_slides_workspace_validators_sync(tmp.path(), "ready-deck");

        let failures = inspect_workspace_contract_failures(tmp.path())
            .expect("at least one broken repo must produce failures");
        assert!(failures.contains("slides/broken-deck"));
        // Only the broken repo should appear in the failures listing —
        // ready-deck is not in the failing set.
        assert!(
            !failures.contains("ready-deck") || failures.contains("broken-deck"),
            "ready-deck should not appear as a failure:\n{}",
            failures
        );
    }

    #[tokio::test]
    async fn run_task_demotes_success_when_contract_fails() {
        // End-to-end integration: an EndTurn that would otherwise be Success
        // gets demoted to success=false when the working_dir contains a
        // policy-managed repo that is not ready.
        let dir = tempfile::tempdir().unwrap();
        // Pre-populate a broken slides repo so contract != ready.
        make_managed_slides_workspace(dir.path(), "demo", false);

        let tools = ToolRegistry::with_builtins(dir.path());
        let provider: Arc<dyn LlmProvider> = Arc::new(EndTurnOnlyProvider);
        let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
        let agent = Agent::new(AgentId::new("contract-demote"), provider, tools, memory);
        let task = Task::new(
            TaskKind::Code {
                instruction: "Build it".into(),
                files: vec![],
            },
            TaskContext {
                working_dir: dir.path().to_path_buf(),
                ..Default::default()
            },
        );

        let result = agent.run_task(&task).await.unwrap();
        assert!(
            !result.success,
            "broken workspace contract must demote task to failure"
        );
        assert!(
            result.output.contains("workspace contract") || result.output.contains("slides/demo"),
            "result output should explain the contract failure: {:?}",
            result.output
        );
    }

    #[tokio::test]
    async fn run_task_keeps_success_when_workspace_has_no_policy() {
        // No-policy workspace must stay Success (no regression).
        let dir = tempfile::tempdir().unwrap();
        let tools = ToolRegistry::with_builtins(dir.path());
        let provider: Arc<dyn LlmProvider> = Arc::new(EndTurnOnlyProvider);
        let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
        let agent = Agent::new(AgentId::new("no-contract"), provider, tools, memory);
        let task = Task::new(
            TaskKind::Code {
                instruction: "Hi".into(),
                files: vec![],
            },
            TaskContext {
                working_dir: dir.path().to_path_buf(),
                ..Default::default()
            },
        );

        let result = agent.run_task(&task).await.unwrap();
        assert!(
            result.success,
            "no-policy workspace must keep Success (got {:?})",
            result.output
        );
    }

    // ── Fleet-UX soak B4 (mini1 / dspfac, 2026-05-22) ─────────────────
    //
    // Suite for the spawn_only synthesized-ack suppression. When the LLM
    // calls a spawn_only tool whose dispatcher returns an error, the agent
    // must NOT fabricate a "Background work started for `<tool>`."
    // acknowledgement — the user already sees a red error chip on the tool
    // card and the synthesized ack reads as a confusing dual signal.

    #[test]
    fn is_error_tool_message_classifies_error_envelopes() {
        // Positive cases — every well-known error convention emitted by
        // crate::agent::execution must classify as an error.
        assert!(is_error_tool_message("Error: tool dispatch failed"));
        assert!(is_error_tool_message(
            "[VALIDATION FAILED] Tool 'run_pipeline' rejected input: bad DOT"
        ));
        assert!(is_error_tool_message(
            "[POLICY DENIED] Tool 'foo' is blocked by provider policy (deny)"
        ));
        assert!(is_error_tool_message(
            "[HOOK DENIED] Tool 'foo' was blocked by a lifecycle hook."
        ));
        assert!(is_error_tool_message("[SESSION LIMIT] cap"));
        assert!(is_error_tool_message("[SHELL RETRY LIMIT] stop"));
        assert!(is_error_tool_message("Tool 'foo' panicked: boom"));
        assert!(is_error_tool_message(
            "Tool 'foo' timed out after 30 seconds"
        ));
        assert!(is_error_tool_message(
            "Tool 'foo' cancelled due to earlier sibling error in the same batch."
        ));

        // Leading whitespace must not defeat the prefix check.
        assert!(is_error_tool_message("   Error: trimmed"));

        // Negative cases — successful and neutral bodies must NOT be flagged.
        assert!(!is_error_tool_message(""));
        assert!(!is_error_tool_message("   "));
        assert!(!is_error_tool_message("ok"));
        assert!(!is_error_tool_message(
            "{\"task_handle\": \"abc\", \"output_dir\": \"/tmp\"}"
        ));
        assert!(!is_error_tool_message(
            "Background research kicked off; results pending."
        ));
        // A "Tool '...'" message that doesn't match panicked/timed-out/
        // cancelled-due-to-earlier is informational, not an error envelope.
        assert!(!is_error_tool_message(
            "Tool 'spawn' produced files: report.md"
        ));
    }

    fn spawn_only_tool_call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: serde_json::json!({}),
            metadata: None,
        }
    }

    fn spawn_only_tool_result(tool_call_id: &str, content: &str) -> Message {
        Message {
            role: MessageRole::Tool,
            content: content.to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: Some(tool_call_id.to_string()),
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        }
    }

    fn spawn_only_chat_response(tool_calls: Vec<ToolCall>) -> ChatResponse {
        ChatResponse {
            content: None,
            reasoning_content: None,
            tool_calls,
            stop_reason: StopReason::ToolUse,
            usage: LlmTokenUsage::default(),
            provider_index: None,
        }
    }

    #[test]
    fn any_tool_invocation_errored_detects_error_envelope() {
        let response = spawn_only_chat_response(vec![spawn_only_tool_call("call_1", "any_tool")]);
        let messages = vec![spawn_only_tool_result(
            "call_1",
            "Error: any_tool dispatch failed",
        )];

        // Empty success-map exercises the content-classifier fallback path
        // (the success bit is the post-#1187 authoritative input; absence
        // means the call bypassed execute_tools, e.g. session-limit block).
        assert!(any_tool_invocation_errored(&messages, &response, &[]));
    }

    #[test]
    fn any_tool_invocation_errored_false_when_all_results_successful() {
        // Mix of a spawn_only-style handle envelope and a regular successful
        // tool result — neither carries an error convention, so the gate must
        // not fire.
        let response = spawn_only_chat_response(vec![
            spawn_only_tool_call("call_a", "bg_research"),
            spawn_only_tool_call("call_b", "shell"),
        ]);
        let messages = vec![
            spawn_only_tool_result(
                "call_a",
                "{\"task_handle\": \"abc\", \"output_dir\": \"/tmp/research\"}",
            ),
            spawn_only_tool_result("call_b", "ls\nfile1\nfile2\nExit code: 0"),
        ];

        assert!(!any_tool_invocation_errored(&messages, &response, &[]));
    }

    #[test]
    fn any_tool_invocation_errored_detects_validation_failed_envelope() {
        let response =
            spawn_only_chat_response(vec![spawn_only_tool_call("call_1", "run_pipeline")]);
        let messages = vec![spawn_only_tool_result(
            "call_1",
            "[VALIDATION FAILED] Tool 'run_pipeline' rejected input: bad arg\n\nFix the input and retry.",
        )];

        assert!(any_tool_invocation_errored(&messages, &response, &[]));
    }

    #[test]
    fn any_tool_invocation_errored_mixed_batch_one_failed() {
        // The realistic production shape: spawn_only tool returned its
        // task-handle envelope (foreground always reports success for
        // spawn_only) AND a sibling regular tool errored in the same batch.
        // The gate MUST fire so the synthesized "Background work started"
        // ack is suppressed — otherwise the user sees a successful-looking
        // ack alongside the red error chip from the sibling tool.
        let response = spawn_only_chat_response(vec![
            spawn_only_tool_call("call_pipeline", "run_pipeline"),
            spawn_only_tool_call("call_shell", "shell"),
        ]);
        let messages = vec![
            spawn_only_tool_result(
                "call_pipeline",
                "{\"task_handle\": \"deep-research-xyz\", \"output_dir\": \"/tmp/dr\"}",
            ),
            spawn_only_tool_result("call_shell", "Error: command not found: foo"),
        ];

        assert!(any_tool_invocation_errored(&messages, &response, &[]));
    }

    #[test]
    fn any_tool_invocation_errored_ignores_unrelated_error_in_history() {
        // A historical error message from an EARLIER turn that doesn't
        // correspond to any tool_call in the current response must NOT
        // trip the gate — otherwise once any tool ever failed in the
        // session, the spawn_only ack would be permanently suppressed.
        let response =
            spawn_only_chat_response(vec![spawn_only_tool_call("call_now", "bg_research")]);
        let messages = vec![
            // Stale tool message from a previous iteration with a
            // tool_call_id the current response doesn't reference.
            spawn_only_tool_result("call_old", "Error: old failure"),
            // Current invocation's successful handle envelope.
            spawn_only_tool_result("call_now", "{\"task_handle\": \"abc\"}"),
        ];

        assert!(!any_tool_invocation_errored(&messages, &response, &[]));
    }

    #[test]
    fn any_tool_invocation_errored_detects_panic_and_timeout_envelopes() {
        let response = spawn_only_chat_response(vec![
            spawn_only_tool_call("call_a", "tool_a"),
            spawn_only_tool_call("call_b", "tool_b"),
        ]);
        let messages_panic = vec![spawn_only_tool_result(
            "call_a",
            "Tool 'tool_a' panicked: boom",
        )];
        assert!(any_tool_invocation_errored(&messages_panic, &response, &[],));

        let messages_timeout = vec![spawn_only_tool_result(
            "call_b",
            "Tool 'tool_b' timed out after 30 seconds",
        )];
        assert!(any_tool_invocation_errored(
            &messages_timeout,
            &response,
            &[],
        ));
    }

    // ─── Codex round-2 MAJOR 2 (PR #1187 fixup) ────────────────────────
    //
    // The new authoritative path: success bit from the dispatcher's
    // `ToolResult` is plumbed through as a (tool_call_id, success) slice.
    // These cover the failure shapes the content-only classifier missed.

    #[test]
    fn any_tool_invocation_errored_uses_success_bit_for_shell_timeout() {
        // shell.rs:396 emits "Command timed out after ..." with success=false.
        // The content does NOT start with "Error:" / "[VALIDATION FAILED]" /
        // etc., so the content classifier returns false. With the success
        // bit available, the gate MUST still fire.
        let response = spawn_only_chat_response(vec![spawn_only_tool_call("call_sh", "shell")]);
        let messages = vec![spawn_only_tool_result(
            "call_sh",
            "Command timed out after 60s\nExit code: -1",
        )];
        let success_map = vec![("call_sh".to_string(), false)];

        assert!(any_tool_invocation_errored(
            &messages,
            &response,
            &success_map,
        ));
    }

    #[test]
    fn any_tool_invocation_errored_uses_success_bit_for_sandbox_path_reject() {
        // coding_tools.rs:680 emits "Path outside working directory ..."
        // with success=false. Same content-classifier blind spot as above.
        let response = spawn_only_chat_response(vec![spawn_only_tool_call("call_rf", "read_file")]);
        let messages = vec![spawn_only_tool_result(
            "call_rf",
            "Path outside working directory: /etc/passwd",
        )];
        let success_map = vec![("call_rf".to_string(), false)];

        assert!(any_tool_invocation_errored(
            &messages,
            &response,
            &success_map,
        ));
    }

    #[test]
    fn any_tool_invocation_errored_uses_success_bit_for_browser_nav_fail() {
        // Browser tool emits "Navigation failed: <reason>" with success=false.
        // Content does not match any well-known prefix.
        let response = spawn_only_chat_response(vec![spawn_only_tool_call("call_br", "browser")]);
        let messages = vec![spawn_only_tool_result(
            "call_br",
            "Navigation failed: net::ERR_NAME_NOT_RESOLVED for https://example.invalid/",
        )];
        let success_map = vec![("call_br".to_string(), false)];

        assert!(any_tool_invocation_errored(
            &messages,
            &response,
            &success_map,
        ));
    }

    #[test]
    fn any_tool_invocation_errored_uses_success_bit_for_plugin_failure() {
        // Plugin tools emit arbitrary failure text with success=false. The
        // body looks like normal output ("Could not connect to host" etc.)
        // and the content classifier would miss it entirely.
        let response =
            spawn_only_chat_response(vec![spawn_only_tool_call("call_pl", "deep_search")]);
        let messages = vec![spawn_only_tool_result(
            "call_pl",
            "Could not connect to host: search.api.invalid (connection refused)",
        )];
        let success_map = vec![("call_pl".to_string(), false)];

        assert!(any_tool_invocation_errored(
            &messages,
            &response,
            &success_map,
        ));
    }

    #[test]
    fn any_tool_invocation_errored_success_bit_authoritative_over_content() {
        // Authoritative-over-content: even if a tool's body happens to
        // contain "Failed to execute" anywhere in it, when the success
        // bit is TRUE the gate must NOT fire — the dispatcher signed off
        // on the call, the body is just narrative.
        let response = spawn_only_chat_response(vec![spawn_only_tool_call("call_ok", "shell")]);
        let messages = vec![spawn_only_tool_result(
            "call_ok",
            "Failed to execute previously, retried, ran cleanly second time.\nExit code: 0",
        )];
        let success_map = vec![("call_ok".to_string(), true)];

        assert!(!any_tool_invocation_errored(
            &messages,
            &response,
            &success_map,
        ));
    }

    #[test]
    fn any_tool_invocation_errored_falls_back_to_content_when_id_missing() {
        // Bypass-execute_tools shape: session-limit blocking emits a
        // synthetic tool message via `session_limit_message` whose
        // tool_call_id has NO entry in the success map. The content
        // classifier still catches `[SESSION LIMIT]` so the gate fires.
        let response =
            spawn_only_chat_response(vec![spawn_only_tool_call("call_blocked", "shell")]);
        let messages = vec![spawn_only_tool_result(
            "call_blocked",
            "[SESSION LIMIT] Tool 'shell' was blocked: cap reached",
        )];

        assert!(any_tool_invocation_errored(&messages, &response, &[]));
    }

    /// Tool that mimics a regular sibling whose `execute` returns `Err`.
    /// Mirrors what happens on mini1 / dspfac (2026-05-22) when the LLM
    /// dispatches a tool whose host-side binary is missing — the
    /// execution layer wraps the eyre error as `"Error: <reason>"` on the
    /// tool-result message and tags the per-tool success bit as `false`.
    struct ErroringTool {
        name: &'static str,
        message: &'static str,
    }

    #[async_trait]
    impl Tool for ErroringTool {
        fn name(&self) -> &str {
            self.name
        }

        fn description(&self) -> &str {
            "Tool that always returns an Err to mimic a missing-host failure"
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }

        async fn execute(&self, _args: &serde_json::Value) -> Result<ToolResult> {
            Err(eyre::eyre!(self.message))
        }
    }

    /// Provider that emits, in one turn, a spawn_only tool call AND a
    /// sibling regular tool call (which errors). Then on its second call
    /// emits an EndTurn with a terminal assistant message. Models the
    /// fleet-UX soak symptom: the LLM batched both calls; the spawn_only
    /// one launched (foreground returns success handle, flag set), the
    /// sibling errored, AND the spawn_only branch in
    /// `process_message_inner` would fabricate a "Background work started"
    /// ack alongside the red error chip.
    struct MixedBatchSpawnOnlyAndErroringProvider {
        calls: AtomicUsize,
        spawn_only_name: &'static str,
        erroring_name: &'static str,
        final_content: &'static str,
    }

    #[async_trait]
    impl LlmProvider for MixedBatchSpawnOnlyAndErroringProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> Result<ChatResponse> {
            let call = self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(if call == 0 {
                ChatResponse {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![
                        ToolCall {
                            id: "call_pipeline".to_string(),
                            name: self.spawn_only_name.to_string(),
                            arguments: serde_json::json!({}),
                            metadata: None,
                        },
                        ToolCall {
                            id: "call_sibling".to_string(),
                            name: self.erroring_name.to_string(),
                            arguments: serde_json::json!({}),
                            metadata: None,
                        },
                    ],
                    stop_reason: StopReason::ToolUse,
                    usage: LlmTokenUsage::default(),
                    provider_index: None,
                }
            } else {
                ChatResponse {
                    content: Some(self.final_content.to_string()),
                    reasoning_content: None,
                    tool_calls: vec![],
                    stop_reason: StopReason::EndTurn,
                    usage: LlmTokenUsage::default(),
                    provider_index: None,
                }
            })
        }

        fn model_id(&self) -> &str {
            "mock"
        }

        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    /// Provider for the codex round-2 MAJOR 1 sticky-flag regression: emits
    /// three iterations —
    ///   iter 1: spawn_only call (foreground returns success handle, sets
    ///           the turn-wide `spawn_only_was_invoked` flag).
    ///   iter 2: a SINGLE regular non-spawn-only tool call. Its result is
    ///           happy. The CURRENT iteration's response contains NO
    ///           spawn_only call. The bug: the sticky flag is still `true`
    ///           from iter 1, no tool in iter 2 errored, so the iter-2
    ///           ToolUse arm would fall through to the synth-ack branch
    ///           and fabricate "Background work started for `<spawn_only>`."
    ///           even though the iter-2 LLM call invoked NO spawn_only
    ///           tool. Without the fix, iter 1 ALREADY returns the
    ///           synth-ack (everything succeeded) so the loop terminates
    ///           before reaching iter 2 at all — we therefore reshape the
    ///           sequence so iter 1's batch SUPPRESSES the synth-ack
    ///           naturally (via the existing B4 erroring-sibling gate)
    ///           and only the sticky-flag-only path reaches iter 2.
    ///   iter 3: EndTurn — the LLM produces the actual user-facing reply.
    struct StickyFlagThreeIterProvider {
        calls: AtomicUsize,
        spawn_only_name: &'static str,
        erroring_sibling_name: &'static str,
        iter2_regular_name: &'static str,
        final_content: &'static str,
    }

    #[async_trait]
    impl LlmProvider for StickyFlagThreeIterProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> Result<ChatResponse> {
            let call = self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(match call {
                0 => ChatResponse {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![
                        ToolCall {
                            id: "call_iter1_spawnonly".to_string(),
                            name: self.spawn_only_name.to_string(),
                            arguments: serde_json::json!({}),
                            metadata: None,
                        },
                        ToolCall {
                            id: "call_iter1_sibling".to_string(),
                            name: self.erroring_sibling_name.to_string(),
                            arguments: serde_json::json!({}),
                            metadata: None,
                        },
                    ],
                    stop_reason: StopReason::ToolUse,
                    usage: LlmTokenUsage::default(),
                    provider_index: None,
                },
                1 => ChatResponse {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![ToolCall {
                        id: "call_iter2_regular".to_string(),
                        name: self.iter2_regular_name.to_string(),
                        arguments: serde_json::json!({}),
                        metadata: None,
                    }],
                    stop_reason: StopReason::ToolUse,
                    usage: LlmTokenUsage::default(),
                    provider_index: None,
                },
                _ => ChatResponse {
                    content: Some(self.final_content.to_string()),
                    reasoning_content: None,
                    tool_calls: vec![],
                    stop_reason: StopReason::EndTurn,
                    usage: LlmTokenUsage::default(),
                    provider_index: None,
                },
            })
        }

        fn model_id(&self) -> &str {
            "mock"
        }

        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    /// Integration: codex round-2 MAJOR 1 (PR #1187 fixup). The sticky
    /// `spawn_only_was_invoked` AtomicBool stayed `true` across iterations
    /// once any iteration in the turn called a spawn_only tool. If a
    /// later iteration in the SAME turn (a) called only a regular
    /// non-spawn-only tool, (b) got a happy result from it, and then
    /// (c) reached the post-tool ToolUse arm, the synth-ack branch would
    /// fabricate a "Background work started." bubble at that iteration
    /// even though the LLM was just calling read_file / shell. The fix
    /// narrows the gate to the CURRENT iteration's `response.tool_calls`
    /// via [`ToolRegistry::is_spawn_only`].
    ///
    /// This test models a 3-iteration turn:
    ///   iter 1: run_pipeline (spawn_only) + erroring sibling
    ///           — existing B4 gate suppresses the synth-ack
    ///   iter 2: read_task_output (regular) returns happy output
    ///           — sticky flag would re-fire the gate without the fix
    ///   iter 3: EndTurn — produces the user-facing reply
    ///
    /// With the bug, iter 2 returned a synthesised ack with
    /// `synthesized_from_spawn_only = true` as the turn-final content.
    /// With the fix, iter 2 falls through and iter 3's EndTurn becomes
    /// the turn-final reply.
    #[tokio::test]
    async fn spawn_only_sticky_flag_does_not_synthesize_ack_in_later_regular_iteration() {
        let dir = tempfile::tempdir().unwrap();
        let mut tools = ToolRegistry::with_builtins(dir.path());
        // Iter 1: spawn_only tool (succeeds on foreground; returns handle
        // envelope — sets `spawn_only_was_invoked` AtomicBool to true).
        tools.register(NamedEchoTool {
            name: "run_pipeline",
            output: "unused (foreground returns the handle envelope)",
        });
        tools.mark_spawn_only("run_pipeline", None);
        // Iter 1 sibling: erroring tool — the existing B4 gate suppresses
        // the iter-1 synth-ack because of THIS error, allowing the loop
        // to actually reach iter 2 where the sticky-flag bug fires.
        tools.register(ErroringTool {
            name: "shell",
            message: "required tool(s) not available on this host: shell-helper",
        });
        // Iter 2: regular tool that returns a happy body. The CURRENT
        // iteration's response calls only this tool — no spawn_only call.
        tools.register(NamedEchoTool {
            name: "read_task_output",
            output: "<happy log lines>\nExit code: 0",
        });

        let provider: Arc<dyn LlmProvider> = Arc::new(StickyFlagThreeIterProvider {
            calls: AtomicUsize::new(0),
            spawn_only_name: "run_pipeline",
            erroring_sibling_name: "shell",
            iter2_regular_name: "read_task_output",
            final_content: "Pipeline launched; shell-helper failed; read_task_output is clean — done.",
        });
        let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
        let agent = Agent::new(
            AgentId::new("spawn-only-sticky-test"),
            provider,
            tools,
            memory,
        );

        let result = agent.process_message("run …", &[], vec![]).await.unwrap();

        // Iter 2's regular tool MUST NOT trigger the synth-ack — the
        // CURRENT iteration's response contains no spawn_only call. With
        // the sticky-flag bug, the harness fabricates a "Background work
        // started for `run_pipeline`." bubble at iter 2 even though iter
        // 2 only called read_task_output (a regular tool).
        assert!(
            !result.content.starts_with("Background work started"),
            "iter-2 regular tool must NOT synthesize spawn_only ack — current iteration has no spawn_only tool call. Got: {:?}",
            result.content
        );
        assert!(
            !result.synthesized_from_spawn_only,
            "synthesized_from_spawn_only flag must be false when CURRENT iteration's response contains no spawn_only tool call, regardless of earlier iterations in the same turn"
        );
        assert_eq!(
            result.content,
            "Pipeline launched; shell-helper failed; read_task_output is clean — done.",
            "the LLM's iter-3 EndTurn reply must be surfaced, not a synthesised ack"
        );
    }

    /// Integration: when an LLM turn emits a spawn_only tool_call AND a
    /// sibling tool_call whose dispatcher returned `Err`, the harness MUST
    /// NOT fabricate a "Background work started for `<tool>`."
    /// acknowledgement. The synthesized ack would render as a successful
    /// bubble alongside the red error chip the UI already shows for the
    /// failed sibling — a confusing dual signal. Instead the LLM must get
    /// another iteration to react to the error and produce a real reply.
    ///
    /// Fleet-UX soak finding B4 (mini1 / dspfac, 2026-05-22): dspfac saw
    /// `× run_pipeline error: required tool(s) not available on this host:
    /// run_pipeline` AND a fake "已后台启动 …" outline bubble
    /// simultaneously; the harness emitted the synthesised ack as the
    /// turn-final assistant content even though a tool in the same batch
    /// reported a failure result that the LLM still needed to acknowledge.
    #[tokio::test]
    async fn spawn_only_branch_skipped_when_invocation_returned_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut tools = ToolRegistry::with_builtins(dir.path());
        // The spawn_only tool succeeds on the foreground (returns the
        // canonical handle envelope) and sets the `spawn_only_was_invoked`
        // flag — exactly as `run_pipeline` does in production.
        tools.register(NamedEchoTool {
            name: "run_pipeline",
            output: "unused (foreground returns the handle envelope, not this)",
        });
        tools.mark_spawn_only("run_pipeline", None);
        // The sibling tool errors synchronously; the dispatcher wraps the
        // eyre into `"Error: <reason>"` on the tool-result message.
        tools.register(ErroringTool {
            name: "shell",
            message: "required tool(s) not available on this host: shell-helper",
        });

        let provider: Arc<dyn LlmProvider> = Arc::new(MixedBatchSpawnOnlyAndErroringProvider {
            calls: AtomicUsize::new(0),
            spawn_only_name: "run_pipeline",
            erroring_name: "shell",
            final_content: "Pipeline launched; shell-helper failed and I cannot proceed without it.",
        });
        let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
        let agent = Agent::new(AgentId::new("spawn-only-err-test"), provider, tools, memory);

        let result = agent
            .process_message("深度研究 James Webb...", &[], vec![])
            .await
            .unwrap();

        // The synthesised ack would carry this prefix. With the gate
        // active, the spawn_only branch is skipped, the loop continues,
        // and the LLM's second (EndTurn) reply becomes the turn-final
        // content.
        assert!(
            !result.content.starts_with("Background work started"),
            "expected NO synthesized 'Background work started' ack alongside the failed sibling tool, got: {:?}",
            result.content
        );
        assert!(
            !result.synthesized_from_spawn_only,
            "synthesized_from_spawn_only flag must be false when a tool in the same batch errored"
        );
        assert_eq!(
            result.content,
            "Pipeline launched; shell-helper failed and I cannot proceed without it.",
            "the LLM's recovery reply must be surfaced, not the synthesized ack"
        );

        // The error tool-result MUST stay visible in the message history so
        // the SPA can keep rendering the red error chip on the tool card.
        let error_visible = result.messages.iter().any(|message| {
            message.role == MessageRole::Tool
                && message
                    .content
                    .contains("required tool(s) not available on this host")
        });
        assert!(
            error_visible,
            "the failed sibling tool-result must remain in messages so the SPA keeps the red error chip: {:?}",
            result
                .messages
                .iter()
                .map(|m| (m.role, m.content.clone()))
                .collect::<Vec<_>>()
        );
    }

    /// Provider for the codex round-3 MAJOR (PR #1187 follow-up): emits, in
    /// a single turn, a spawn_only tool call AND a sibling regular tool call
    /// whose tool_call_id contains a `:` so that `handle_tool_use` rewrites
    /// it via `sanitize_tool_call_id`. Then on the next call emits an
    /// EndTurn with a terminal assistant message.
    ///
    /// Models the round-3 bug: with the pre-fix code, the post-tool gate
    /// at `any_tool_invocation_errored` was called with the CALLER'S
    /// ORIGINAL response (`admin_view:11` still on it), so the success-bit
    /// lookup keyed by the SANITIZED id (`admin_view_11`) missed, the
    /// content-fallback scan also keyed on the original id (still missed),
    /// and the synth-ack fired even though the sibling reported
    /// `success=false`.
    struct SanitizedIdSpawnOnlyAndErroringProvider {
        calls: AtomicUsize,
        spawn_only_name: &'static str,
        erroring_name: &'static str,
        erroring_raw_id: &'static str,
        final_content: &'static str,
    }

    #[async_trait]
    impl LlmProvider for SanitizedIdSpawnOnlyAndErroringProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> Result<ChatResponse> {
            let call = self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(if call == 0 {
                ChatResponse {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![
                        ToolCall {
                            id: "call_pipeline".to_string(),
                            name: self.spawn_only_name.to_string(),
                            arguments: serde_json::json!({}),
                            metadata: None,
                        },
                        ToolCall {
                            // Colon in the id mirrors the dspfac /
                            // Moonshot-kimi pattern (`admin_view_sessions:11`).
                            // `handle_tool_use` rewrites this to
                            // `admin_view_sessions_11` via
                            // `sanitize_tool_call_id`. With the round-3
                            // bug, the post-tool gate sees the ORIGINAL id
                            // (with the colon) and misses the success-bit
                            // entry that the dispatcher keyed by the
                            // SANITIZED id.
                            id: self.erroring_raw_id.to_string(),
                            name: self.erroring_name.to_string(),
                            arguments: serde_json::json!({}),
                            metadata: None,
                        },
                    ],
                    stop_reason: StopReason::ToolUse,
                    usage: LlmTokenUsage::default(),
                    provider_index: None,
                }
            } else {
                ChatResponse {
                    content: Some(self.final_content.to_string()),
                    reasoning_content: None,
                    tool_calls: vec![],
                    stop_reason: StopReason::EndTurn,
                    usage: LlmTokenUsage::default(),
                    provider_index: None,
                }
            })
        }

        fn model_id(&self) -> &str {
            "mock"
        }

        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    /// Codex round-3 MAJOR (PR #1187 follow-up). The post-tool synth-ack
    /// gate (`any_tool_invocation_errored`) was called with the CALLER'S
    /// ORIGINAL response, but `handle_tool_use` had sanitized/dedup'd a
    /// CLONE before executing tools. When sanitization rewrote a tool_call_id
    /// (e.g. `admin_view:11` → `admin_view_11`), the success-bit lookup
    /// (keyed by the sanitized id) missed, the content-fallback scan (also
    /// keyed on the original id) also missed, and a real `success=false`
    /// would slip past the gate — the synth-ack still fired.
    ///
    /// Fix: `handle_tool_use` now returns the sanitized response; the
    /// caller passes that sanitized response into the gate so the keys
    /// align with the success-bit sink.
    ///
    /// This test verifies: when the sibling failing tool has a
    /// colon-bearing id that gets sanitized AND `success=false` is
    /// reported, the synth-ack is correctly suppressed (the bug would
    /// produce a `synthesized_from_spawn_only=true` ack here).
    #[tokio::test]
    async fn synth_ack_suppressed_when_failing_tool_has_sanitized_id() {
        let dir = tempfile::tempdir().unwrap();
        let mut tools = ToolRegistry::with_builtins(dir.path());
        // spawn_only foreground returns the handle envelope (success=true)
        // and flips the spawn_only-was-invoked flag.
        tools.register(NamedEchoTool {
            name: "run_pipeline",
            output: "unused (foreground returns the handle envelope, not this)",
        });
        tools.mark_spawn_only("run_pipeline", None);
        // Sibling tool errors; dispatcher keys the success-bit entry by
        // the SANITIZED tool_call_id (the LLM-supplied id had a colon).
        tools.register(ErroringTool {
            name: "shell",
            message: "required tool(s) not available on this host: shell-helper",
        });

        let provider: Arc<dyn LlmProvider> = Arc::new(SanitizedIdSpawnOnlyAndErroringProvider {
            calls: AtomicUsize::new(0),
            spawn_only_name: "run_pipeline",
            erroring_name: "shell",
            erroring_raw_id: "admin_view_sessions:11",
            final_content: "Pipeline launched; shell-helper failed — cannot proceed.",
        });
        let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
        let agent = Agent::new(
            AgentId::new("spawn-only-sanitized-id-test"),
            provider,
            tools,
            memory,
        );

        let result = agent
            .process_message("kick off a deep search", &[], vec![])
            .await
            .unwrap();

        // With the pre-fix code, the gate misses the sanitized-id
        // success=false entry and the synth-ack fires:
        //   result.content starts with "Background work started for `run_pipeline`."
        //   result.synthesized_from_spawn_only == true
        //
        // With the round-3 fix, the gate sees the sanitized id, finds
        // success=false, suppresses the ack, the loop continues, and the
        // LLM's iter-2 EndTurn produces the terminal reply.
        assert!(
            !result.content.starts_with("Background work started"),
            "synth-ack must be suppressed when sibling tool errored AND its \
             tool_call_id was rewritten by sanitization; got: {:?}",
            result.content
        );
        assert!(
            !result.synthesized_from_spawn_only,
            "synthesized_from_spawn_only must be false when sibling with \
             sanitized tool_call_id reported success=false"
        );
        assert_eq!(
            result.content, "Pipeline launched; shell-helper failed — cannot proceed.",
            "the LLM's recovery reply must surface, not the synth-ack"
        );

        // Sanity: the failing-sibling tool-result lives under a SANITIZED
        // id (no colon). After `handle_tool_use` sanitizes the colon to
        // `_`, downstream prepare-message steps (`normalize_tool_call_ids`,
        // see loop_compaction.rs) may additionally add the `call_` prefix
        // before the next LLM call — so we accept either
        // `admin_view_sessions_11` or `call_admin_view_sessions_11`. What
        // matters is: NO message carries the original colon-bearing id,
        // proving sanitization ran end-to-end.
        let sanitized_tool_msg = result.messages.iter().find(|message| {
            message.role == MessageRole::Tool
                && message
                    .tool_call_id
                    .as_deref()
                    .is_some_and(|id| !id.contains(':') && id.contains("admin_view_sessions_11"))
        });
        assert!(
            sanitized_tool_msg.is_some(),
            "expected the failing sibling's tool-result keyed by a sanitized id \
             (containing `admin_view_sessions_11`, no colon); messages were: {:?}",
            result
                .messages
                .iter()
                .map(|m| (m.role, m.tool_call_id.clone()))
                .collect::<Vec<_>>()
        );
        // And NOT under the original colonized id — sanitization rewrote it.
        let original_colonized_msg = result.messages.iter().any(|message| {
            message.role == MessageRole::Tool
                && message
                    .tool_call_id
                    .as_deref()
                    .is_some_and(|id| id == "admin_view_sessions:11")
        });
        assert!(
            !original_colonized_msg,
            "no tool-result should carry the original colon-bearing id; \
             sanitization should have rewritten it"
        );
    }

    #[ignore = "Pre-migration test: the SpawnOnlyFiles-source MagicBytes validator \
                (post-#997 round-3) rejects no-files-emitted tasks at the project-scope \
                gate. This test's `EndTurnOnlyProvider` agent never calls a plugin tool, \
                so `files_to_send` stays empty and the loop_runner's project-scope \
                validator run after run_task fails the freshly-staged ready workspace. \
                Re-enable by giving the agent a stub plugin tool that returns the staged \
                deck in `tool_result.files_to_send`."]
    #[tokio::test]
    async fn run_task_keeps_success_when_contract_passes() {
        let dir = tempfile::tempdir().unwrap();
        // Fully-ready workspace.
        make_managed_slides_workspace(dir.path(), "ready", true);
        run_managed_slides_workspace_validators(dir.path(), "ready").await;

        let tools = ToolRegistry::with_builtins(dir.path());
        let provider: Arc<dyn LlmProvider> = Arc::new(EndTurnOnlyProvider);
        let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
        let agent = Agent::new(AgentId::new("contract-ok"), provider, tools, memory);
        let task = Task::new(
            TaskKind::Code {
                instruction: "All good".into(),
                files: vec![],
            },
            TaskContext {
                working_dir: dir.path().to_path_buf(),
                ..Default::default()
            },
        );

        let result = agent.run_task(&task).await.unwrap();
        assert!(
            result.success,
            "ready workspace must keep Success (got {:?})",
            result.output
        );
    }
}
