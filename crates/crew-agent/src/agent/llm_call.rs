//! LLM call orchestration with lifecycle hooks and retry logic.

use std::time::{Duration, Instant};

use crew_core::Message;
use crew_core::TokenUsage;
use crew_llm::{ChatConfig, ChatResponse, StopReason, ToolSpec};
use eyre::Result;
use tracing::warn;

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
            let call_result = async {
                let stream = self.llm.chat_stream(messages, tools_spec, config).await?;
                self.consume_stream(stream, iteration).await
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
                            let pricing = crew_llm::pricing::model_pricing(self.llm.model_id());
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
                        // All retries exhausted with empty/filtered response -- report
                        // failure to the adaptive router so it can failover, then
                        // return error.
                        let reason = if response.stop_reason == StopReason::ContentFiltered {
                            "content filtered by safety/moderation"
                        } else {
                            "empty response (no content or tool_calls)"
                        };
                        self.llm.report_late_failure();
                        warn!(
                            attempts = Self::LLM_RETRY_MAX + 1,
                            reason,
                            "LLM returned empty response after all retries, triggering failover"
                        );
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
                    } else {
                        // Non-retryable error or last attempt -- propagate
                        return Err(e);
                    }
                }
            }
        }

        // All retries exhausted with errors
        Err(last_error.unwrap_or_else(|| eyre::eyre!("LLM call failed after retries")))
    }
}
