//! Tool execution: dispatching tool calls with hooks and timeout handling.

use std::time::{Duration, Instant};

use eyre::Result;
use octos_core::{Message, MessageRole, TokenUsage};
use octos_llm::ChatResponse;
use tracing::{debug, info, warn};

use super::{Agent, MAX_TOOL_TIMEOUT_SECS};
use crate::hooks::{HookEvent, HookPayload, HookResult};
use crate::progress::ProgressEvent;
use crate::tools::{TOOL_CTX, ToolContext};

impl Agent {
    pub(super) async fn execute_tools(
        &self,
        response: &ChatResponse,
    ) -> Result<(Vec<Message>, Vec<std::path::PathBuf>, TokenUsage)> {
        // Log parallel tool execution details
        let tool_names: Vec<&str> = response
            .tool_calls
            .iter()
            .map(|tc| tc.name.as_str())
            .collect();
        tracing::info!(
            parallel_tools = response.tool_calls.len(),
            tool_names = %tool_names.join(", "),
            "executing tools in parallel"
        );

        // Spawn each tool as a separate tokio task so that if the agent-level
        // timeout fires, the tasks keep running and can perform their own cleanup
        // (e.g., browser tool kills Chrome, spawn tool finishes gracefully).
        // Without tokio::spawn, timeout would drop the futures mid-flight,
        // orphaning child processes (Chrome, shell commands, etc.).
        let handles: Vec<_> = response
            .tool_calls
            .iter()
            .map(|tool_call| {
                // Clone Arc-wrapped fields so the spawned task is 'static
                let tools = self.tools.clone();
                let reporter = self.reporter();
                let hooks = self.hooks.clone();
                let hook_ctx = self.hook_ctx();
                let tc_name = tool_call.name.clone();
                let tc_id = tool_call.id.clone();
                let tc_args = tool_call.arguments.clone();

                tokio::spawn(async move {
                    let tool_start = Instant::now();
                    debug!(tool = %tc_name, tool_id = %tc_id, "executing tool");

                    reporter.report(ProgressEvent::ToolStarted {
                        name: tc_name.clone(),
                        tool_id: tc_id.clone(),
                    });

                    // Before-tool hook: may deny or modify args
                    let mut effective_args = tc_args.clone();
                    if let Some(ref hooks) = hooks {
                        let payload = HookPayload::before_tool(
                            &tc_name,
                            tc_args.clone(),
                            &tc_id,
                            hook_ctx.as_ref(),
                        );
                        match hooks.run(HookEvent::BeforeToolCall, &payload).await {
                            HookResult::Deny(reason) => {
                                tracing::warn!(
                                    tool = %tc_name,
                                    reason = %reason,
                                    "before_tool_call hook denied"
                                );
                                let deny_msg = if reason.is_empty() {
                                    format!("[HOOK DENIED] Tool '{}' was blocked by a lifecycle hook. Do not retry.", tc_name)
                                } else {
                                    format!("[HOOK DENIED] Tool '{}' was blocked: {}. Do not retry.", tc_name, reason)
                                };
                                return (
                                    Message {
                                        role: MessageRole::Tool,
                                        content: deny_msg,
                                        media: vec![],
                                        tool_calls: None,
                                        tool_call_id: Some(tc_id),
                                        reasoning_content: None,
                                        timestamp: chrono::Utc::now(),
                                    },
                                    None,
                                    None,
                                );
                            }
                            HookResult::Modified(new_args) => {
                                tracing::info!(
                                    tool = %tc_name,
                                    "hook modified tool arguments"
                                );
                                effective_args = new_args;
                            }
                            _ => {}
                        }
                    }

                    let ctx = ToolContext {
                        tool_id: tc_id.clone(),
                        reporter: reporter.clone(),
                    };
                    let result = TOOL_CTX
                        .scope(ctx, tools.execute(&tc_name, &effective_args))
                        .await;

                    let duration = tool_start.elapsed();

                    let (content, file_modified, tool_tokens, tool_success) = match result {
                        Ok(tool_result) => {
                            debug!(
                                tool = %tc_name,
                                success = tool_result.success,
                                duration_ms = duration.as_millis() as u64,
                                "tool completed"
                            );

                            if let Some(ref file) = tool_result.file_modified {
                                info!(tool = %tc_name, file = %file.display(), "file modified");
                                reporter.report(ProgressEvent::FileModified {
                                    path: file.display().to_string(),
                                });
                            }

                            // Auto-send files: explicit files_to_send OR detected file paths in output.
                            // This acts as a universal post-tool hook — any plugin that produces
                            // a deliverable file gets it sent to the user automatically.
                            let mut files: Vec<String> = tool_result.files_to_send
                                .iter()
                                .map(|p| p.to_string_lossy().to_string())
                                .collect();

                            // Detect file paths in output for plugins that don't set files_to_send
                            if files.is_empty() && tool_result.success {
                                let sendable_extensions = [
                                    ".mp3", ".wav", ".ogg", ".flac",  // audio
                                    ".pptx", ".pdf", ".xlsx",         // documents
                                    ".png", ".jpg", ".jpeg", ".svg",  // images
                                ];
                                for word in tool_result.output.split_whitespace() {
                                    let lower = word.to_lowercase();
                                    if sendable_extensions.iter().any(|ext| lower.ends_with(ext)) {
                                        // Strip trailing punctuation
                                        let cleaned = word.trim_end_matches(|c: char| c == '.' || c == ',' || c == ')' || c == ']');
                                        if std::path::Path::new(cleaned).exists() {
                                            files.push(cleaned.to_string());
                                        }
                                    }
                                }
                            }

                            for path_str in &files {
                                info!(tool = %tc_name, file = %path_str, "auto-sending file to user");
                                let send_args = serde_json::json!({"file_path": path_str});
                                match tools.execute("send_file", &send_args).await {
                                    Ok(r) if r.success => {
                                        info!(tool = %tc_name, file = %path_str, "file auto-sent");
                                    }
                                    Ok(r) => {
                                        warn!(tool = %tc_name, file = %path_str, error = %r.output, "auto-send failed");
                                    }
                                    Err(e) => {
                                        warn!(tool = %tc_name, file = %path_str, error = %e, "auto-send failed");
                                    }
                                }
                            }

                            let output_preview =
                                octos_core::truncated_utf8(&tool_result.output, 200, "...");

                            reporter.report(ProgressEvent::ToolCompleted {
                                name: tc_name.clone(),
                                tool_id: tc_id.clone(),
                                success: tool_result.success,
                                output_preview,
                                duration,
                            });

                            let success = tool_result.success;
                            (
                                tool_result.output,
                                tool_result.file_modified,
                                tool_result.tokens_used,
                                success,
                            )
                        }
                        Err(e) => {
                            warn!(
                                tool = %tc_name,
                                error = %e,
                                duration_ms = duration.as_millis() as u64,
                                "tool failed"
                            );

                            reporter.report(ProgressEvent::ToolCompleted {
                                name: tc_name.clone(),
                                tool_id: tc_id.clone(),
                                success: false,
                                output_preview: e.to_string(),
                                duration,
                            });

                            (format!("Error: {e}"), None, None, false)
                        }
                    };

                    // After-tool hook (fire-and-forget)
                    if let Some(ref hooks) = hooks {
                        let payload = HookPayload::after_tool(
                            &tc_name,
                            &tc_id,
                            octos_core::truncated_utf8(&content, 500, "..."),
                            tool_success,
                            duration.as_millis() as u64,
                            hook_ctx.as_ref(),
                        );
                        let _ = hooks.run(HookEvent::AfterToolCall, &payload).await;
                    }

                    // Per-tool output truncation with head/tail split
                    let limit = octos_core::tool_output_limit(&tc_name);
                    let content = octos_core::truncate_head_tail(&content, limit, 0.7);
                    let content = crate::sanitize::sanitize_tool_output(&content);

                    (
                        Message {
                            role: MessageRole::Tool,
                            content,
                            media: vec![],
                            tool_calls: None,
                            tool_call_id: Some(tc_id),
                            reasoning_content: None,
                            timestamp: chrono::Utc::now(),
                        },
                        file_modified,
                        tool_tokens,
                    )
                })
            })
            .collect();

        // Let the LLM specify per-tool timeout via `timeout_secs` in tool call args.
        // Use the max of all requested timeouts, clamped to MAX_TOOL_TIMEOUT_SECS.
        let llm_requested_timeout: u64 = response
            .tool_calls
            .iter()
            .filter_map(|tc| tc.arguments.get("timeout_secs").and_then(|v| v.as_u64()))
            .max()
            .unwrap_or(0);
        let tool_timeout_secs = if llm_requested_timeout > 0 {
            llm_requested_timeout
                .min(MAX_TOOL_TIMEOUT_SECS)
                .max(self.config.tool_timeout_secs)
        } else {
            self.config.tool_timeout_secs
        };
        let tool_timeout = Duration::from_secs(tool_timeout_secs);
        let results: Vec<_> =
            match tokio::time::timeout(tool_timeout, futures::future::join_all(handles)).await {
                Ok(results) => {
                    // Unwrap JoinHandle results -- panics in tool tasks become errors
                    results
                        .into_iter()
                        .zip(response.tool_calls.iter())
                        .map(|(r, tc)| {
                            r.unwrap_or_else(|e| {
                                // Task panicked -- return error with tool_call_id so
                                // the LLM knows which tool failed.
                                (
                                    Message {
                                        role: MessageRole::Tool,
                                        content: format!("Tool '{}' panicked: {e}", tc.name),
                                        media: vec![],
                                        tool_calls: None,
                                        tool_call_id: Some(tc.id.clone()),
                                        reasoning_content: None,
                                        timestamp: chrono::Utc::now(),
                                    },
                                    None,
                                    None,
                                )
                            })
                        })
                        .collect()
                }
                Err(_) => {
                    tracing::error!(
                        timeout_secs = tool_timeout_secs,
                        tool_count = response.tool_calls.len(),
                        tools = %tool_names.join(", "),
                        "tool execution timed out -- spawned tasks continue running for cleanup"
                    );
                    // Note: spawned tasks are NOT aborted -- they continue running so
                    // tools can perform their own cleanup (browser kills Chrome, etc.).
                    // They will eventually complete via their own internal timeouts.
                    let mut messages = Vec::with_capacity(response.tool_calls.len());
                    for tc in &response.tool_calls {
                        messages.push(Message {
                            role: MessageRole::Tool,
                            content: format!(
                                "Tool '{}' timed out after {} seconds",
                                tc.name, tool_timeout_secs
                            ),
                            media: vec![],
                            tool_calls: None,
                            tool_call_id: Some(tc.id.clone()),
                            reasoning_content: None,
                            timestamp: chrono::Utc::now(),
                        });
                    }
                    return Ok((messages, vec![], TokenUsage::default()));
                }
            };

        // Log completion of all parallel tools
        let result_sizes: Vec<usize> = results.iter().map(|(m, _, _)| m.content.len()).collect();
        let total_result_bytes: usize = result_sizes.iter().sum();
        tracing::info!(
            parallel_tools = results.len(),
            result_sizes = %result_sizes.iter().map(|s| s.to_string()).collect::<Vec<_>>().join(", "),
            total_result_bytes,
            "all parallel tools completed"
        );

        // Aggregate results -- join_all preserves input order.
        let mut messages = Vec::with_capacity(results.len());
        let mut files_modified = Vec::new();
        let mut tokens_used = TokenUsage::default();

        for (message, file_modified, tool_tokens) in results {
            messages.push(message);
            if let Some(file) = file_modified {
                files_modified.push(file);
            }
            if let Some(tokens) = tool_tokens {
                tokens_used.input_tokens += tokens.input_tokens;
                tokens_used.output_tokens += tokens.output_tokens;
            }
        }

        Ok((messages, files_modified, tokens_used))
    }
}
