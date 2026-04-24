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
use crate::session::SessionLimits;
use crate::tools::{TURN_ATTACHMENT_CTX, TurnAttachmentContext};

const MAX_PARALLEL_TOOL_CALLS_PER_BATCH: usize = 8;
const SHELL_RETRY_RECOVERY_THRESHOLD: usize = 4;

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
enum ShellRetryRecoveryKind {
    DiffLikeSuccess,
    UsefulSuccess,
    ValidationSuccess,
    RetryLimit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShellRetryRecovery {
    kind: ShellRetryRecoveryKind,
    content: String,
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
    ) -> Option<String> {
        let recovery = recover_shell_retry(messages, SHELL_RETRY_RECOVERY_THRESHOLD)?;
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
        Some(recovery.content)
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
    #[allow(dead_code)]
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
            BudgetStop::MaxIterations | BudgetStop::MaxTokens { .. }
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

                messages.push(Message {
                    role: MessageRole::User,
                    content,
                    media,
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: None,
                    timestamp: chrono::Utc::now(),
                });

                let config = self.chat_config();
                let mut files_modified = Vec::new();
                let mut files_to_send = Vec::new();
                let mut turn = LoopTurnState::new(Instant::now());
                // M6.2: per-turn retry-bucket state machine. Lives alongside
                // `LoopTurnState` rather than inside it so the file boundary
                // from issue #489 stays exact.
                let mut retry_state = LoopRetryState::new();
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
                                messages: LoopTurnState::new_messages(&messages, history.len()),
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
                                    let _ = self.classify_loop_error(&e, None);
                                    return Err(e);
                                }
                            }
                        }
                        Err(e) => {
                            let _ = self.classify_loop_error(&e, None);
                            return Err(e);
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
                            self.emit_cost_update(turn.total_usage(), &response.usage);
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
                                messages: LoopTurnState::new_messages(&messages, history.len()),
                            });
                        }
                        StopReason::ToolUse => {
                            // Check for loop detection before executing
                            for tc in &response.tool_calls {
                                if let Some(warning) = loop_detector.record(&tc.name, &tc.arguments)
                                {
                                    warn!("loop detected — breaking agent loop");
                                    let spiral_iteration = turn.iteration();
                                    if let Some(recovered_content) = self
                                        .dispatch_shell_retry_recovery(
                                            &messages,
                                            &mut retry_state,
                                            spiral_iteration,
                                        )
                                    {
                                        warn!(
                                            "loop detected after repeated shell attempts; returning recovered shell output"
                                        );
                                        self.emit_cost_update(turn.total_usage(), &response.usage);
                                        return Ok(ConversationResponse {
                                            content: recovered_content,
                                            reasoning_content: None,
                                            provider_metadata: None,
                                            token_usage: turn.total_usage().clone(),
                                            files_modified,
                                            files_to_send,
                                            streamed,
                                            messages: LoopTurnState::new_messages(
                                                &messages,
                                                history.len(),
                                            ),
                                        });
                                    }
                                    // Don't execute the tools — break out with a message
                                    self.emit_cost_update(turn.total_usage(), &response.usage);
                                    return Ok(ConversationResponse {
                                        content: warning,
                                        reasoning_content: None,
                                        provider_metadata: None,
                                        token_usage: turn.total_usage().clone(),
                                        files_modified,
                                        files_to_send,
                                        streamed,
                                        messages: LoopTurnState::new_messages(
                                            &messages,
                                            history.len(),
                                        ),
                                    });
                                }
                            }
                            if let Err(e) = self
                                .handle_tool_use(
                                    &response,
                                    &mut messages,
                                    &mut files_modified,
                                    Some(&mut files_to_send),
                                    &mut turn,
                                    &mut retry_state,
                                    tracker,
                                )
                                .await
                            {
                                let _ = self.classify_loop_error(&e, None);
                                return Err(e);
                            }

                            let spiral_iteration = turn.iteration();
                            if let Some(recovered_content) = self.dispatch_shell_retry_recovery(
                                &messages,
                                &mut retry_state,
                                spiral_iteration,
                            ) {
                                warn!(
                                    "ending turn after repeated shell attempts with recovered shell output"
                                );
                                self.emit_cost_update(turn.total_usage(), &response.usage);
                                return Ok(ConversationResponse {
                                    content: recovered_content,
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
                                    messages: LoopTurnState::new_messages(
                                        &messages,
                                        history.len(),
                                    ),
                                });
                            }

                            if self.tools.spawn_only_was_invoked() {
                                self.emit_cost_update(turn.total_usage(), &response.usage);
                                let background_tools = response
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
                                    messages: LoopTurnState::new_messages(&messages, history.len()),
                                });
                            }
                        }
                        StopReason::MaxTokens => {
                            self.emit_cost_update(turn.total_usage(), &response.usage);
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
                                messages: LoopTurnState::new_messages(&messages, history.len()),
                            });
                        }
                        StopReason::ContentFiltered => {
                            // After retries in call_llm_with_hooks, content is still filtered.
                            // Return a user-visible message instead of empty content.
                            self.emit_cost_update(turn.total_usage(), &response.usage);
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
                                messages: LoopTurnState::new_messages(&messages, history.len()),
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
            // M6.2: per-run retry-bucket state machine. Same instance lives
            // across all iterations of the task loop so bucket counters
            // accumulate the way operators expect.
            let mut retry_state = LoopRetryState::new();
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
                        let _ = self.classify_loop_error(&e, None);
                        return Err(e);
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
                        if self.config.save_episodes {
                            let summary = response.content.clone().unwrap_or_default();
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

                        self.emit_cost_update(turn.total_usage(), &response.usage);
                        self.reporter().report(ProgressEvent::TaskCompleted {
                            success: true,
                            iterations: iteration,
                            duration: task_start.elapsed(),
                        });

                        info!(
                            total_input_tokens = turn.total_usage().input_tokens,
                            total_output_tokens = turn.total_usage().output_tokens,
                            iterations = iteration,
                            files_modified = files_modified.len(),
                            duration_ms = task_start.elapsed().as_millis() as u64,
                            "task completed"
                        );
                        return Ok(self.build_result(
                            &response,
                            turn.total_usage().clone(),
                            files_modified,
                            files_to_send,
                        ));
                    }
                    StopReason::ToolUse => {
                        if let Err(e) = self
                            .handle_tool_use(
                                &response,
                                &mut messages,
                                &mut files_modified,
                                Some(&mut files_to_send),
                                &mut turn,
                                &mut retry_state,
                                None,
                            )
                            .await
                        {
                            let _ = self.classify_loop_error(&e, None);
                            return Err(e);
                        }
                    }
                    StopReason::MaxTokens => {
                        self.emit_cost_update(turn.total_usage(), &response.usage);
                        self.reporter().report(ProgressEvent::TaskCompleted {
                            success: false,
                            iterations: iteration,
                            duration: task_start.elapsed(),
                        });
                        return Ok(self.build_result(
                            &response,
                            turn.total_usage().clone(),
                            files_modified,
                            files_to_send,
                        ));
                    }
                    StopReason::ContentFiltered => {
                        warn!("content filtered by provider safety/moderation in task");
                        self.emit_cost_update(turn.total_usage(), &response.usage);
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
        let success = response.stop_reason != StopReason::MaxTokens;
        TaskResult {
            schema_version: octos_core::TASK_RESULT_SCHEMA_VERSION,
            success,
            output: response.content.clone().unwrap_or_default(),
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
    ) -> Result<()> {
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
        messages.push(self.response_to_message(&response));
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
        for batch in tool_batches {
            let mut batch_response = limited_response.clone();
            batch_response.tool_calls = batch.to_vec();
            let (batch_messages, batch_files, batch_send_files, batch_tokens) =
                self.execute_tools(&batch_response).await?;
            tool_messages.extend(batch_messages);
            tool_files.extend(batch_files);
            tool_send_files.extend(batch_send_files);
            tool_tokens.input_tokens += batch_tokens.input_tokens;
            tool_tokens.output_tokens += batch_tokens.output_tokens;
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

        messages.extend(merged);
        files_modified.extend(tool_files);
        if let Some(files_to_send) = files_to_send {
            files_to_send.extend(tool_send_files);
        }
        turn.record_usage(tool_tokens.input_tokens, tool_tokens.output_tokens, tracker);
        Ok(())
    }
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
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    use async_trait::async_trait;
    use octos_core::{AgentId, MessageRole, TaskContext, TaskKind, ToolCall};
    use octos_llm::{ChatResponse, LlmProvider, StopReason, TokenUsage as LlmTokenUsage};
    use octos_memory::EpisodeStore;

    use crate::plugins::PluginTool;
    use crate::plugins::manifest::PluginToolDef;
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
            spawn_only_message: None,
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
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "fatal: not a git repository\n\nExit code: 128".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_1".into()),
                reasoning_content: None,
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
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "diff --git a/notes.txt b/notes.txt\n--- a/notes.txt\n+++ b/notes.txt\n@@ -1,2 +1,2 @@\n alpha\n-beta\n+gamma\n\nExit code: 0".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_2".into()),
                reasoning_content: None,
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
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "(no output)\n\nExit code: 0".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_3".into()),
                reasoning_content: None,
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
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "fatal: not a git repository\n\nExit code: 128".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_4".into()),
                reasoning_content: None,
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
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "test result: FAILED. 0 passed; 1 failed\n\nExit code: 101".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_1".into()),
                reasoning_content: None,
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
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "updated src/lib.rs".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_edit_1".into()),
                reasoning_content: None,
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
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "test result: FAILED. 0 passed; 1 failed\n\nExit code: 101".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_2".into()),
                reasoning_content: None,
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
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "test result: ok. 1 passed; 0 failed\n\nExit code: 0".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_3".into()),
                reasoning_content: None,
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
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-buggy\n+fixed\n\nExit code: 0".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_4".into()),
                reasoning_content: None,
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
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "error: first failure\n\nExit code: 101".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_1".into()),
                reasoning_content: None,
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
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "error: second failure\n\nExit code: 101".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_2".into()),
                reasoning_content: None,
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
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: " M src/lib.rs\n?? notes.txt\n\nExit code: 0".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_3".into()),
                reasoning_content: None,
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
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "error: third failure\n\nExit code: 101".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_4".into()),
                reasoning_content: None,
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
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "Initialized empty Git repository in /tmp/repo/.git/\n\nExit code: 0".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_1".into()),
                reasoning_content: None,
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
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "[master (root-commit) 1e19620] initial commit\n 1 file changed, 2 insertions(+)\n create mode 100644 notes.txt\n\nExit code: 0".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_2".into()),
                reasoning_content: None,
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
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "fatal: ambiguous argument 'notes.txt'\n\nExit code: 128".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_3".into()),
                reasoning_content: None,
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
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "/tmp\n\nExit code: 0".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_4".into()),
                reasoning_content: None,
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
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "error: first failure\n\nExit code: 101".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_1".into()),
                reasoning_content: None,
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
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "error: second failure\n\nExit code: 101".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_2".into()),
                reasoning_content: None,
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
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "test result: ok. 1 passed; 0 failed\n\nExit code: 0".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_3".into()),
                reasoning_content: None,
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
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "error: third failure\n\nExit code: 101".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_4".into()),
                reasoning_content: None,
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
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "/tmp/octos\n\nExit code: 0".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_1".into()),
                reasoning_content: None,
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
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "lib.rs\nmain.rs\n\nExit code: 0".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_2".into()),
                reasoning_content: None,
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
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: " M src/lib.rs\n?? notes.txt\n\nExit code: 0".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_3".into()),
                reasoning_content: None,
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
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "[package]\nname = \"octos\"\n\nExit code: 0".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_4".into()),
                reasoning_content: None,
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
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "error: could not find Cargo.toml\n\nExit code: 101".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_1".into()),
                reasoning_content: None,
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
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "error: could not find Cargo.toml\n\nExit code: 101".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_2".into()),
                reasoning_content: None,
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
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "error: could not find Cargo.toml\n\nExit code: 101".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_3".into()),
                reasoning_content: None,
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
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "error: could not find Cargo.toml\n\nExit code: 101".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("call_shell_4".into()),
                reasoning_content: None,
                timestamp: chrono::Utc::now(),
            },
        ];

        let recovered = recover_shell_retry(&messages, 4).expect("should stop");
        assert_eq!(recovered.kind, ShellRetryRecoveryKind::RetryLimit);
        assert!(recovered.content.contains("[SHELL RETRY LIMIT]"));
        assert!(recovered.content.contains("could not find Cargo.toml"));
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
}
