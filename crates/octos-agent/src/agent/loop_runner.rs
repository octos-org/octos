//! Main agent loop: process_message and run_task orchestration.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Instant;

use eyre::Result;
use octos_core::{Message, MessageRole, Task, TaskResult, TokenUsage};
use octos_llm::{ChatConfig, ChatResponse, StopReason};
use octos_memory::{Episode, EpisodeOutcome};
use tracing::{Instrument, info, info_span, warn};

use super::message_repair::{
    normalize_system_messages, normalize_tool_call_ids, repair_message_order, repair_tool_pairs,
    sanitize_tool_call_id, synthesize_missing_tool_results, truncate_old_tool_results,
};
use super::{Agent, ConversationResponse, TokenTracker};
use crate::loop_detect::LoopDetector;
use crate::progress::ProgressEvent;

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
        self.process_message_inner(user_content, history, media, None)
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
        self.process_message_inner(user_content, history, media, Some(tracker))
            .await
    }

    async fn process_message_inner(
        &self,
        user_content: &str,
        history: &[Message],
        media: Vec<String>,
        tracker: Option<&TokenTracker>,
    ) -> Result<ConversationResponse> {
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

        let content = if user_content.is_empty() && !media.is_empty() {
            "[User sent an image]".to_string()
        } else {
            user_content.to_string()
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
        let mut total_usage = TokenUsage::default();
        let mut files_modified = Vec::new();
        let mut iteration = 0u32;
        let start = Instant::now();
        let mut loop_detector = LoopDetector::new(12);

        loop {
            if let Some(stop) = self.check_budget(iteration, start, &total_usage) {
                // Skip system prompt + history; return only new messages
                let new_start = (1 + history.len()).min(messages.len());
                return Ok(ConversationResponse {
                    content: stop.message(),
                    reasoning_content: None,
                    token_usage: total_usage,
                    files_modified,
                    streamed: false,
                    messages: messages[new_start..].to_vec(),
                });
            }

            iteration += 1;
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
            self.trim_to_context_window(&mut messages);
            normalize_system_messages(&mut messages);
            repair_message_order(&mut messages);
            repair_tool_pairs(&mut messages);
            synthesize_missing_tool_results(&mut messages);
            truncate_old_tool_results(&mut messages);
            normalize_tool_call_ids(&mut messages);

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
                .call_llm_with_hooks(&messages, &tools_spec, &config, iteration, &total_usage)
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
                        &total_usage,
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
            total_usage.input_tokens += response.usage.input_tokens;
            total_usage.output_tokens += response.usage.output_tokens;
            if let Some(t) = tracker {
                t.input_tokens
                    .store(total_usage.input_tokens, Ordering::Relaxed);
                t.output_tokens
                    .store(total_usage.output_tokens, Ordering::Relaxed);
            }

            match response.stop_reason {
                StopReason::EndTurn | StopReason::StopSequence => {
                    self.emit_cost_update(&total_usage, &response.usage);
                    let new_start = (1 + history.len()).min(messages.len());
                    return Ok(ConversationResponse {
                        content: response.content.unwrap_or_default(),
                        reasoning_content: response.reasoning_content.clone(),
                        token_usage: total_usage,
                        files_modified,
                        streamed,
                        messages: messages[new_start..].to_vec(),
                    });
                }
                StopReason::ToolUse => {
                    // Check for loop detection before executing
                    for tc in &response.tool_calls {
                        if let Some(warning) = loop_detector.record(&tc.name, &tc.arguments) {
                            warn!("loop detected in tool calls");
                            messages.push(Message {
                                role: MessageRole::System,
                                content: warning,
                                media: vec![],
                                tool_calls: None,
                                tool_call_id: None,
                                reasoning_content: None,
                                timestamp: chrono::Utc::now(),
                            });
                        }
                    }
                    self.handle_tool_use(
                        &response,
                        &mut messages,
                        &mut files_modified,
                        &mut total_usage,
                        tracker,
                    )
                    .await?;
                }
                StopReason::MaxTokens => {
                    self.emit_cost_update(&total_usage, &response.usage);
                    let new_start = (1 + history.len()).min(messages.len());
                    return Ok(ConversationResponse {
                        content: response.content.unwrap_or_default(),
                        reasoning_content: response.reasoning_content.clone(),
                        token_usage: total_usage,
                        files_modified,
                        streamed,
                        messages: messages[new_start..].to_vec(),
                    });
                }
                StopReason::ContentFiltered => {
                    // After retries in call_llm_with_hooks, content is still filtered.
                    // Return a user-visible message instead of empty content.
                    self.emit_cost_update(&total_usage, &response.usage);
                    warn!("content filtered by provider safety/moderation after retries");
                    let new_start = (1 + history.len()).min(messages.len());
                    return Ok(ConversationResponse {
                        content: response.content.unwrap_or_else(|| {
                            "[Content was blocked by the model's safety filter. \
                             Please rephrase your request.]"
                                .to_string()
                        }),
                        reasoning_content: None,
                        token_usage: total_usage,
                        files_modified,
                        streamed,
                        messages: messages[new_start..].to_vec(),
                    });
                }
            }
        }
    }

    /// Run a task to completion (used by spawn tool).
    pub async fn run_task(&self, task: &Task) -> Result<TaskResult> {
        let task_start = Instant::now();
        let span = info_span!(
            "task",
            task_id = %task.id,
            agent_id = %self.id,
        );

        async {
            info!("starting task");
            self.reporter().report(ProgressEvent::TaskStarted {
                task_id: task.id.to_string(),
            });

            let mut iteration = 0u32;
            let mut messages = self.build_initial_messages(task).await;
            let mut total_usage = TokenUsage::default();
            let mut files_modified = Vec::new();
            let config = self.chat_config();

            loop {
                if let Some(stop) = self.check_budget(iteration, task_start, &total_usage) {
                    self.report_budget_stop(&stop, iteration);
                    return Ok(TaskResult {
                        success: false,
                        output: stop.message(),
                        files_modified,
                        subtasks: Vec::new(),
                        token_usage: total_usage,
                    });
                }

                iteration += 1;
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
                self.trim_to_context_window(&mut messages);
                normalize_tool_call_ids(&mut messages);

                let (response, _streamed) = self
                    .call_llm_with_hooks(&messages, &tools_spec, &config, iteration, &total_usage)
                    .await?;
                total_usage.input_tokens += response.usage.input_tokens;
                total_usage.output_tokens += response.usage.output_tokens;

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

                        self.emit_cost_update(&total_usage, &response.usage);
                        self.reporter().report(ProgressEvent::TaskCompleted {
                            success: true,
                            iterations: iteration,
                            duration: task_start.elapsed(),
                        });

                        info!(
                            total_input_tokens = total_usage.input_tokens,
                            total_output_tokens = total_usage.output_tokens,
                            iterations = iteration,
                            files_modified = files_modified.len(),
                            duration_ms = task_start.elapsed().as_millis() as u64,
                            "task completed"
                        );
                        return Ok(self.build_result(&response, total_usage, files_modified));
                    }
                    StopReason::ToolUse => {
                        self.handle_tool_use(
                            &response,
                            &mut messages,
                            &mut files_modified,
                            &mut total_usage,
                            None,
                        )
                        .await?;
                    }
                    StopReason::MaxTokens => {
                        self.emit_cost_update(&total_usage, &response.usage);
                        self.reporter().report(ProgressEvent::TaskCompleted {
                            success: false,
                            iterations: iteration,
                            duration: task_start.elapsed(),
                        });
                        return Ok(self.build_result(&response, total_usage, files_modified));
                    }
                    StopReason::ContentFiltered => {
                        warn!("content filtered by provider safety/moderation in task");
                        self.emit_cost_update(&total_usage, &response.usage);
                        self.reporter().report(ProgressEvent::TaskCompleted {
                            success: false,
                            iterations: iteration,
                            duration: task_start.elapsed(),
                        });
                        let mut result = self.build_result(&response, total_usage, files_modified);
                        if result.output.is_empty() {
                            result.output =
                                "[Content was blocked by the model's safety filter.]".to_string();
                        }
                        return Ok(result);
                    }
                }
            }
        }
        .instrument(span)
        .await
    }

    fn build_result(
        &self,
        response: &ChatResponse,
        usage: TokenUsage,
        files_modified: Vec<std::path::PathBuf>,
    ) -> TaskResult {
        let success = response.stop_reason != StopReason::MaxTokens;
        TaskResult {
            success,
            output: response.content.clone().unwrap_or_default(),
            files_modified,
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
        total_usage: &mut TokenUsage,
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
        let (tool_messages, tool_files, tool_tokens) = self.execute_tools(&response).await?;
        messages.extend(tool_messages);
        files_modified.extend(tool_files);
        total_usage.input_tokens += tool_tokens.input_tokens;
        total_usage.output_tokens += tool_tokens.output_tokens;
        if let Some(t) = tracker {
            t.input_tokens
                .store(total_usage.input_tokens, Ordering::Relaxed);
            t.output_tokens
                .store(total_usage.output_tokens, Ordering::Relaxed);
        }
        Ok(())
    }
}
