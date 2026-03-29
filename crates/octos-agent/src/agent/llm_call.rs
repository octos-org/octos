//! LLM call orchestration with lifecycle hooks and retry logic.

use std::time::{Duration, Instant};

use eyre::Result;
use octos_core::Message;
use octos_core::TokenUsage;
use octos_llm::{ChatConfig, ChatResponse, StopReason, ToolSpec};
use tracing::{info, warn};

use super::Agent;
use crate::hooks::{HookEvent, HookPayload, HookResult};
use crate::progress::ProgressEvent;

impl Agent {
    /// Maximum retries for transient LLM failures (empty responses, stream errors).
    const LLM_RETRY_MAX: u32 = 3;

    /// Call the LLM with before/after lifecycle hooks.
    /// Automatically retries on empty responses and retryable stream errors.
    pub(super) async fn call_llm_with_hooks(
        &self,
        messages: &[Message],
        tools_spec: &[ToolSpec],
        config: &ChatConfig,
        iteration: u32,
        total_usage: &TokenUsage,
    ) -> Result<(ChatResponse, bool)> {
        let ctx = self.hook_ctx();
        if let Some(ref hooks) = self.hooks {
            let payload = HookPayload::before_llm(
                self.llm.model_id(),
                messages.len(),
                iteration,
                ctx.as_ref(),
            );
            if let HookResult::Deny(reason) = hooks.run(HookEvent::BeforeLlmCall, &payload).await {
                eyre::bail!("LLM call denied by hook: {reason}");
            }
        }

        let mut last_error: Option<eyre::Report> = None;
        // Track token usage from retried (discarded) attempts so cost reporting
        // reflects actual consumption, not just the final successful call.
        let mut retry_usage = TokenUsage::default();

        for attempt in 0..=Self::LLM_RETRY_MAX {
            let call_start = Instant::now();
            // Try the full LLM call (stream creation + consumption)
            // Estimate input tokens from message bytes (rough: ~4 chars per token
            // for English, ~1.5 for CJK). Use bytes/3 as a conservative estimate.
            let input_bytes: usize = messages.iter().map(|m| m.content.len()).sum();
            let input_estimate = (input_bytes / 3) as u32;

            let call_result = async {
                let stream = self.llm.chat_stream(messages, tools_spec, config).await?;
                self.consume_stream_with_input_estimate(stream, iteration, input_estimate)
                    .await
            }
            .await;

            match call_result {
                Ok((response, streamed)) => {
                    if !Self::is_retriable_response(&response) {
                        // Genuine success -- merge retry usage into response
                        let mut response = response;
                        response.usage.input_tokens += retry_usage.input_tokens;
                        response.usage.output_tokens += retry_usage.output_tokens;

                        if let Some(ref hooks) = self.hooks {
                            let latency_ms = call_start.elapsed().as_millis() as u64;
                            let cum_in = total_usage.input_tokens + response.usage.input_tokens;
                            let cum_out = total_usage.output_tokens + response.usage.output_tokens;
                            let pricing = octos_llm::pricing::model_pricing(self.llm.model_id());
                            let session_cost = pricing.map(|p| p.cost(cum_in, cum_out));
                            let response_cost = pricing.map(|p| {
                                p.cost(response.usage.input_tokens, response.usage.output_tokens)
                            });
                            let payload = HookPayload::after_llm(
                                self.llm.model_id(),
                                iteration,
                                &format!("{:?}", response.stop_reason),
                                !response.tool_calls.is_empty(),
                                response.usage.input_tokens,
                                response.usage.output_tokens,
                                self.llm.provider_name(),
                                latency_ms,
                                cum_in,
                                cum_out,
                                session_cost,
                                response_cost,
                                ctx.as_ref(),
                            );
                            let _ = hooks.run(HookEvent::AfterLlmCall, &payload).await;
                        }
                        return Ok((response, streamed));
                    }

                    if attempt == Self::LLM_RETRY_MAX {
                        // All streaming retries exhausted. Try one final non-streaming
                        // call — this goes through FallbackProvider.chat() which tries
                        // all fallback providers, not just the primary.
                        let reason = if response.stop_reason == StopReason::ContentFiltered {
                            "content filtered by safety/moderation"
                        } else {
                            "empty response (no content or tool_calls)"
                        };
                        self.llm.report_late_failure();
                        warn!(
                            attempts = Self::LLM_RETRY_MAX + 1,
                            reason, "streaming retries exhausted, trying non-streaming fallback"
                        );

                        // Non-streaming call triggers FallbackProvider's full fallback chain
                        match self.llm.chat(messages, tools_spec, config).await {
                            Ok(fallback_resp) if !Self::is_retriable_response(&fallback_resp) => {
                                info!("non-streaming fallback succeeded");
                                let mut fallback_resp = fallback_resp;
                                fallback_resp.usage.input_tokens += retry_usage.input_tokens;
                                fallback_resp.usage.output_tokens += retry_usage.output_tokens;
                                return Ok((fallback_resp, false));
                            }
                            Ok(_) => {
                                warn!("non-streaming fallback also returned empty response");
                            }
                            Err(e) => {
                                warn!(error = %e, "non-streaming fallback failed");
                            }
                        }

                        return Err(eyre::eyre!(
                            "LLM returned empty response after {} retries: {}",
                            Self::LLM_RETRY_MAX + 1,
                            reason
                        ));
                    }

                    // Empty or abnormal response -- accumulate usage and retry
                    retry_usage.input_tokens += response.usage.input_tokens;
                    retry_usage.output_tokens += response.usage.output_tokens;

                    let delay = Duration::from_secs(1 << attempt);
                    let reason = if response.stop_reason == StopReason::ContentFiltered {
                        "content filtered by safety/moderation"
                    } else {
                        "empty response (no content/tool_calls)"
                    };
                    warn!(
                        attempt = attempt + 1,
                        max = Self::LLM_RETRY_MAX,
                        delay_s = delay.as_secs(),
                        iteration,
                        stop_reason = ?response.stop_reason,
                        reason,
                        "abnormal LLM response, retrying"
                    );
                    // Clear stream forwarder buffer before retry so partial
                    // text from this attempt isn't concatenated with the next.
                    self.reporter()
                        .report(ProgressEvent::StreamRetry { iteration });
                    self.reporter().report(ProgressEvent::LlmStatus {
                        message: format!(
                            "Retrying ({}/{})... {}",
                            attempt + 1,
                            Self::LLM_RETRY_MAX + 1,
                            reason,
                        ),
                        iteration,
                    });
                    tokio::time::sleep(delay).await;
                }
                Err(e) => {
                    if attempt < Self::LLM_RETRY_MAX && Self::is_retryable_stream_error(&e) {
                        let delay = Duration::from_secs(1 << attempt);
                        warn!(
                            attempt = attempt + 1,
                            max = Self::LLM_RETRY_MAX,
                            delay_s = delay.as_secs(),
                            error = %e,
                            iteration,
                            "retryable stream error, retrying"
                        );
                        // Clear stream forwarder buffer before retry so partial
                        // text from this attempt isn't concatenated with the next.
                        self.reporter()
                            .report(ProgressEvent::StreamRetry { iteration });
                        self.reporter().report(ProgressEvent::LlmStatus {
                            message: format!(
                                "Retrying ({}/{})... stream error",
                                attempt + 1,
                                Self::LLM_RETRY_MAX + 1,
                            ),
                            iteration,
                        });
                        last_error = Some(e);
                        tokio::time::sleep(delay).await;
                    } else if attempt == Self::LLM_RETRY_MAX {
                        // Stream retries exhausted — try non-streaming with full fallback chain
                        self.llm.report_late_failure();
                        warn!(
                            error = %e,
                            "stream retries exhausted, trying non-streaming fallback"
                        );
                        match self.llm.chat(messages, tools_spec, config).await {
                            Ok(resp) if !Self::is_retriable_response(&resp) => {
                                info!("non-streaming fallback succeeded after stream failures");
                                return Ok((resp, false));
                            }
                            Ok(_) => {
                                warn!("non-streaming fallback also returned empty");
                            }
                            Err(fb_err) => {
                                warn!(error = %fb_err, "non-streaming fallback also failed");
                            }
                        }
                        return Err(e);
                    } else {
                        // Non-retryable error -- propagate immediately
                        return Err(e);
                    }
                }
            }
        }

        // All retries exhausted with errors
        Err(last_error.unwrap_or_else(|| eyre::eyre!("LLM call failed after retries")))
    }
}
