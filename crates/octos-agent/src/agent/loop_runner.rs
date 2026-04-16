//! Main agent loop: process_message and run_task orchestration.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use eyre::Result;
use octos_core::{Message, MessageRole, Task, TaskResult, TokenUsage};
use octos_llm::{ChatConfig, ChatResponse, StopReason};
use octos_memory::{Episode, EpisodeOutcome};
use tracing::{Instrument, info, info_span, warn};

use super::activity::{ActivityTrackingReporter, LoopActivityState};
use super::loop_compaction::{prepare_conversation_messages, prepare_task_messages};
use super::message_repair::sanitize_tool_call_id;
use super::turn_state::LoopTurnState;
use super::{Agent, ConversationResponse, TASK_REPORTER, TokenTracker};
use crate::loop_detect::LoopDetector;
use crate::progress::ProgressEvent;
use crate::tools::{TURN_ATTACHMENT_CTX, TurnAttachmentContext};

impl Agent {
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

                let mut messages = vec![Message {
                    role: MessageRole::System,
                    content: self
                        .system_prompt
                        .read()
                        .unwrap_or_else(|e| {
                            tracing::warn!("system prompt lock was poisoned, recovering");
                            e.into_inner()
                        })
                        .clone(),
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
                let mut turn = LoopTurnState::new(Instant::now());
                let mut loop_detector = LoopDetector::new(12);

                loop {
                    if let Some(stop) = turn.check_budget(self, activity.as_ref()) {
                        turn.record_budget_stop(&stop);
                        // Skip system prompt + history; return only new messages
                        return Ok(ConversationResponse {
                            content: stop.message(),
                            reasoning_content: None,
                            provider_metadata: None,
                            token_usage: turn.total_usage().clone(),
                            files_modified,
                            streamed: false,
                            messages: LoopTurnState::new_messages(&messages, history.len()),
                        });
                    }

                    let iteration = turn.advance_iteration();
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
                    prepare_conversation_messages(self, &mut messages);

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
                    let (response, streamed) = match self
                        .call_llm_with_hooks(
                            &messages,
                            &tools_spec,
                            &config,
                            iteration,
                            turn.total_usage(),
                        )
                        .await
                    {
                        Ok(r) => r,
                        Err(e) if e.to_string().contains("empty response after") => {
                            // Empty response after retries -- try once more (adaptive router
                            // may select a different provider on this second attempt).
                            warn!(error = %e, "retrying LLM call for adaptive failover");
                            self.reporter().report(ProgressEvent::LlmStatus {
                                message: "Switching provider...".to_string(),
                                iteration,
                            });
                            self.call_llm_with_hooks(
                                &messages,
                                &tools_spec,
                                &config,
                                iteration,
                                turn.total_usage(),
                            )
                            .await?
                        }
                        Err(e) => return Err(e),
                    };
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
                                    // Don't execute the tools — break out with a message
                                    self.emit_cost_update(turn.total_usage(), &response.usage);
                                    return Ok(ConversationResponse {
                                        content: warning,
                                        reasoning_content: None,
                                        provider_metadata: None,
                                        token_usage: turn.total_usage().clone(),
                                        files_modified,
                                        streamed,
                                        messages: LoopTurnState::new_messages(
                                            &messages,
                                            history.len(),
                                        ),
                                    });
                                }
                            }
                            self.handle_tool_use(
                                &response,
                                &mut messages,
                                &mut files_modified,
                                None,
                                &mut turn,
                                tracker,
                            )
                            .await?;

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
            let config = self.chat_config();

            loop {
                if let Some(stop) = turn.check_budget(self, activity.as_ref()) {
                    turn.record_budget_stop(&stop);
                    self.report_budget_stop(&stop, turn.iteration());
                    return Ok(TaskResult {
                        success: false,
                        output: stop.message(),
                        files_modified,
                        files_to_send,
                        subtasks: Vec::new(),
                        token_usage: turn.total_usage().clone(),
                    });
                }

                let iteration = turn.advance_iteration();
                let iter_start = Instant::now();
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
                prepare_task_messages(self, &mut messages);

                let (response, _streamed) = self
                    .call_llm_with_hooks(
                        &messages,
                        &tools_spec,
                        &config,
                        iteration,
                        turn.total_usage(),
                    )
                    .await?;
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
                        self.handle_tool_use(
                            &response,
                            &mut messages,
                            &mut files_modified,
                            Some(&mut files_to_send),
                            &mut turn,
                            None,
                        )
                        .await?;
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
    async fn handle_tool_use(
        &self,
        response: &ChatResponse,
        messages: &mut Vec<Message>,
        files_modified: &mut Vec<PathBuf>,
        files_to_send: Option<&mut Vec<PathBuf>>,
        turn: &mut LoopTurnState,
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
        let (tool_messages, tool_files, tool_send_files, tool_tokens) =
            self.execute_tools(&response).await?;
        messages.extend(tool_messages);
        files_modified.extend(tool_files);
        if let Some(files_to_send) = files_to_send {
            files_to_send.extend(tool_send_files);
        }
        turn.record_usage(tool_tokens.input_tokens, tool_tokens.output_tokens, tracker);
        Ok(())
    }
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

    use crate::tools::{Tool, ToolRegistry, ToolResult};

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
        let roles: Vec<MessageRole> = result.messages.iter().map(|m| m.role.clone()).collect();
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
}
