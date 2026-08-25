//! LLM call orchestration with lifecycle hooks and retry logic.

use std::time::{Duration, Instant};

use eyre::Result;
use octos_core::Message;
use octos_core::TokenUsage;
use octos_llm::{ChatConfig, ChatResponse, LlmCallPolicy, StopReason, ToolSpec};
use tracing::{info, warn};

use super::Agent;
use super::turn_state::{LoopRetryReason, LoopTurnState};
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
        turn: &mut LoopTurnState,
        // Returns `(response, streamed, attributed_cost_usd)`. The response's
        // `usage` MERGES discarded retry attempts into the final attempt, and
        // those attempts can come from DIFFERENT provider slots (an empty
        // stream falling through to a fallback). `attributed_cost_usd` prices
        // each attempt's tokens at the provider that actually consumed them,
        // so callers must record it instead of re-pricing the merged total at
        // the winner's rate (codex #1632 P2). `None` = no attempt was priced.
    ) -> Result<(ChatResponse, bool, Option<f64>)> {
        // Measurement only (#pi/dsh append-only study): report whether this
        // turn's request history is still a prefix-extension of the last one.
        // Off unless OCTOS_APPEND_ONLY_AUDIT=1, and never alters the request —
        // a rewrite here means the sent history stopped being reconstructable
        // from what came before, which is the drift that makes a resumed
        // session differ from the one the model actually had.
        if crate::agent::append_only_audit::enabled() {
            let rewrites = {
                let mut audit = self
                    .append_only_audit
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                audit.observe(messages)
            };
            for rewrite in rewrites {
                let description = rewrite.describe();
                warn!(
                    iteration,
                    model = %self.llm.model_id(),
                    "append-only audit: {description}"
                );
                // Also recorded out-of-band under test: a `tracing` line with
                // no subscriber installed is not a measurement you can verify.
                #[cfg(test)]
                crate::agent::append_only_audit::record_finding(format!(
                    "iteration {iteration}: {description}"
                ));
            }
        }

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
        // Spend already attributed to DISCARDED attempts, each priced at the
        // provider slot that produced it (see the return-value doc above).
        let mut retry_spend: Option<f64> = None;

        let fail_fast = octos_llm::current_llm_call_policy() == LlmCallPolicy::FailFast;
        let retry_max = if fail_fast { 0 } else { Self::LLM_RETRY_MAX };

        // #1712: after a truncated tool call (the turn hit the output cap
        // mid-call), the NEXT attempt requests with the model's full output
        // budget so the call has room to complete — retrying the same capped
        // request would just re-truncate. Only populated on a truncation retry;
        // the happy path never clones the config.
        let mut bumped_config: Option<ChatConfig> = None;

        for attempt in 0..=retry_max {
            let call_start = Instant::now();
            // Try the full LLM call (stream creation + consumption)
            // Estimate input tokens from message bytes (rough: ~4 chars per token
            // for English, ~1.5 for CJK). Use bytes/3 as a conservative estimate.
            let input_bytes: usize = messages.iter().map(|m| m.content.len()).sum();
            let input_estimate = (input_bytes / 3) as u32;

            let attempt_config: &ChatConfig = bumped_config.as_ref().unwrap_or(config);
            let build_and_consume = async {
                let stream = self
                    .llm
                    .chat_stream(messages, tools_spec, attempt_config)
                    .await?;
                self.consume_stream_with_input_estimate(stream, iteration, input_estimate)
                    .await
            };
            // Voice fail-fast: bound the WHOLE {build + consume} future with the
            // voice overall deadline. The per-chunk `StreamTimeouts` only start
            // inside `consume_stream`, so a provider that hangs while returning
            // response headers would otherwise inherit the long production
            // request timeout. Normal turns keep that long backstop unchanged.
            let call_result = if fail_fast {
                match tokio::time::timeout(self.config.voice_overall_deadline, build_and_consume)
                    .await
                {
                    Ok(r) => r,
                    Err(_elapsed) => Err(octos_llm::LlmError::timeout(format!(
                        "voice overall deadline exceeded after {}s",
                        self.config.voice_overall_deadline.as_secs()
                    ))
                    .into()),
                }
            } else {
                build_and_consume.await
            };

            match call_result {
                Ok((response, streamed)) => {
                    if !Self::is_retriable_response(&response) {
                        // Genuine success -- merge retry usage into response.
                        // Price the FINAL attempt at its own slot BEFORE the
                        // merge, then add the pre-priced retry spend.
                        let final_cost = self.response_usage_cost(
                            response.usage.input_tokens,
                            response.usage.output_tokens,
                            response.provider_index,
                        );
                        let attributed_cost = match (retry_spend, final_cost) {
                            (None, None) => None,
                            (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
                        };
                        let mut response = response;
                        response.usage.input_tokens += retry_usage.input_tokens;
                        response.usage.output_tokens += retry_usage.output_tokens;
                        response.usage.cache_read_tokens += retry_usage.cache_read_tokens;
                        response.usage.cache_write_tokens += retry_usage.cache_write_tokens;

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
                        return Ok((response, streamed, attributed_cost));
                    }

                    if attempt == retry_max {
                        // All streaming retries exhausted.
                        let reason = if response.stop_reason == StopReason::ContentFiltered {
                            "content filtered by safety/moderation"
                        } else {
                            "empty response (no content or tool_calls)"
                        };
                        turn.record_retry(LoopRetryReason::ProviderFailover {
                            reason: format!("streaming retries exhausted: {reason}"),
                        });
                        self.llm.report_late_failure();

                        if fail_fast {
                            // FailFast: skip the non-streaming fallback, return error directly.
                            return Err(eyre::eyre!(
                                "LLM returned empty response after {} retries: {}",
                                retry_max + 1,
                                reason
                            ));
                        }

                        // Try one final non-streaming call — this goes through
                        // FallbackProvider.chat() which tries all fallback providers,
                        // not just the primary.
                        warn!(
                            attempts = Self::LLM_RETRY_MAX + 1,
                            reason, "streaming retries exhausted, trying non-streaming fallback"
                        );

                        // Non-streaming call triggers FallbackProvider's full fallback chain
                        match self.llm.chat(messages, tools_spec, config).await {
                            Ok(fallback_resp) if !Self::is_retriable_response(&fallback_resp) => {
                                info!("non-streaming fallback succeeded");
                                let final_cost = self.response_usage_cost(
                                    fallback_resp.usage.input_tokens,
                                    fallback_resp.usage.output_tokens,
                                    fallback_resp.provider_index,
                                );
                                let attributed_cost = match (retry_spend, final_cost) {
                                    (None, None) => None,
                                    (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
                                };
                                let mut fallback_resp = fallback_resp;
                                fallback_resp.usage.input_tokens += retry_usage.input_tokens;
                                fallback_resp.usage.output_tokens += retry_usage.output_tokens;
                                fallback_resp.usage.cache_read_tokens +=
                                    retry_usage.cache_read_tokens;
                                fallback_resp.usage.cache_write_tokens +=
                                    retry_usage.cache_write_tokens;
                                return Ok((fallback_resp, false, attributed_cost));
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

                    // Empty or abnormal response -- accumulate usage and retry.
                    // Price THIS attempt at the slot that produced it now;
                    // by the time a later attempt wins, the winning slot's
                    // rate would misprice these tokens.
                    if let Some(cost) = self.response_usage_cost(
                        response.usage.input_tokens,
                        response.usage.output_tokens,
                        response.provider_index,
                    ) {
                        retry_spend = Some(retry_spend.unwrap_or(0.0) + cost);
                    }
                    retry_usage.input_tokens += response.usage.input_tokens;
                    retry_usage.output_tokens += response.usage.output_tokens;
                    retry_usage.cache_read_tokens += response.usage.cache_read_tokens;
                    retry_usage.cache_write_tokens += response.usage.cache_write_tokens;

                    let delay = Duration::from_secs(1 << attempt);
                    let reason = if response.stop_reason == StopReason::ContentFiltered {
                        "content filtered by safety/moderation"
                    } else {
                        "empty response (no content/tool_calls)"
                    };
                    turn.record_retry(LoopRetryReason::EmptyResponse {
                        attempt: attempt + 1,
                        reason: reason.to_string(),
                    });
                    warn!(
                        attempt = attempt + 1,
                        max = retry_max,
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
                            retry_max + 1,
                            reason,
                        ),
                        iteration,
                    });
                    tokio::time::sleep(delay).await;
                }
                Err(e) => {
                    if attempt < retry_max && Self::is_retryable_stream_error(&e) {
                        let delay = Duration::from_secs(1 << attempt);
                        // #1712: a truncated tool call means the model needed
                        // more output room than the per-turn cap allowed. Retry
                        // with the model's full output budget so the next
                        // attempt can complete the call. Only bump upward.
                        if Self::is_truncated_tool_call_error(&e) {
                            let model_max = self.llm.max_output_tokens();
                            let current = config.max_tokens.unwrap_or(0);
                            if model_max > current {
                                let mut c = config.clone();
                                c.max_tokens = Some(model_max);
                                warn!(
                                    from = current,
                                    to = model_max,
                                    iteration,
                                    "truncated tool call — raising output budget for retry (#1712)"
                                );
                                bumped_config = Some(c);
                            }
                        }
                        turn.record_retry(LoopRetryReason::StreamError {
                            attempt: attempt + 1,
                            error: e.to_string(),
                        });
                        warn!(
                            attempt = attempt + 1,
                            max = retry_max,
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
                                retry_max + 1,
                            ),
                            iteration,
                        });
                        last_error = Some(e);
                        tokio::time::sleep(delay).await;
                    } else if attempt == retry_max {
                        // Stream retries exhausted.
                        turn.record_retry(LoopRetryReason::ProviderFailover {
                            reason: "stream retries exhausted".to_string(),
                        });
                        self.llm.report_late_failure();

                        if fail_fast {
                            // FailFast: skip the non-streaming fallback, return error directly.
                            return Err(e);
                        }

                        // Try non-streaming with full fallback chain
                        warn!(
                            error = %e,
                            "stream retries exhausted, trying non-streaming fallback"
                        );
                        match self.llm.chat(messages, tools_spec, config).await {
                            Ok(resp) if !Self::is_retriable_response(&resp) => {
                                info!("non-streaming fallback succeeded after stream failures");
                                // Codex #1632 r2 P2: merge the accumulated
                                // retry usage here like the empty-response
                                // fallback does — earlier EMPTY attempts (not
                                // just stream errors) can be behind us on
                                // this path, and `retry_spend` already prices
                                // them; returning fallback-only tokens next
                                // to a spend that includes both would skew
                                // the persisted token/cost pairing.
                                let final_cost = self.response_usage_cost(
                                    resp.usage.input_tokens,
                                    resp.usage.output_tokens,
                                    resp.provider_index,
                                );
                                let attributed_cost = match (retry_spend, final_cost) {
                                    (None, None) => None,
                                    (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
                                };
                                let mut resp = resp;
                                resp.usage.input_tokens += retry_usage.input_tokens;
                                resp.usage.output_tokens += retry_usage.output_tokens;
                                resp.usage.cache_read_tokens += retry_usage.cache_read_tokens;
                                resp.usage.cache_write_tokens += retry_usage.cache_write_tokens;
                                return Ok((resp, false, attributed_cost));
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    use super::super::AgentConfig;

    use async_trait::async_trait;
    use futures::stream;
    use octos_core::{AgentId, Message};
    use octos_llm::{
        ChatConfig, ChatResponse, ChatStream, LlmCallPolicy, LlmProvider, StopReason, StreamEvent,
        TokenUsage as LlmTokenUsage, ToolSpec, with_llm_call_policy,
    };
    use octos_memory::EpisodeStore;

    use super::super::Agent;
    use super::super::turn_state::LoopTurnState;
    use crate::tools::ToolRegistry;

    // ── Shared call counters ──────────────────────────────────────────────────

    #[derive(Default)]
    struct CallCounters {
        chat_stream: AtomicU32,
        chat: AtomicU32,
    }

    // ── Provider that always errors on chat_stream (retryable 503) ───────────

    struct AlwaysErrStreamProvider {
        counters: Arc<CallCounters>,
    }

    #[async_trait]
    impl LlmProvider for AlwaysErrStreamProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> eyre::Result<ChatResponse> {
            self.counters.chat.fetch_add(1, Ordering::SeqCst);
            eyre::bail!("non-streaming fallback should not be called under FailFast")
        }

        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> eyre::Result<ChatStream> {
            self.counters.chat_stream.fetch_add(1, Ordering::SeqCst);
            // Return a stream that immediately yields a retryable 503 error.
            let events: Vec<StreamEvent> = vec![StreamEvent::Done(StopReason::EndTurn)];
            let _ = events; // unused — we error at stream creation level
            Err(eyre::eyre!("503 server error: stream unavailable"))
        }

        fn model_id(&self) -> &str {
            "mock-always-err"
        }

        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    // ── Provider that returns empty response (no content, no tool_calls) ──────

    struct AlwaysEmptyProvider {
        counters: Arc<CallCounters>,
    }

    #[async_trait]
    impl LlmProvider for AlwaysEmptyProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> eyre::Result<ChatResponse> {
            self.counters.chat.fetch_add(1, Ordering::SeqCst);
            eyre::bail!("non-streaming fallback should not be called under FailFast")
        }

        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> eyre::Result<ChatStream> {
            self.counters.chat_stream.fetch_add(1, Ordering::SeqCst);
            // Return a stream that yields an empty (retriable) response.
            let events = vec![
                StreamEvent::Usage(LlmTokenUsage::default()),
                StreamEvent::Done(StopReason::EndTurn),
            ];
            Ok(Box::pin(stream::iter(events)))
        }

        fn model_id(&self) -> &str {
            "mock-always-empty"
        }

        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    // ── Provider that hangs forever at stream creation (build phase) ──────────

    struct HangingBuildProvider;

    #[async_trait]
    impl LlmProvider for HangingBuildProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> eyre::Result<ChatResponse> {
            eyre::bail!("non-streaming fallback should not be called under FailFast")
        }

        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> eyre::Result<ChatStream> {
            // Never resolves: simulates a provider that accepts the POST but
            // never returns response headers (build phase hangs). The voice
            // overall deadline must bound this, since `StreamTimeouts` only
            // starts ticking once `consume_stream` runs.
            std::future::pending::<()>().await;
            unreachable!()
        }

        fn model_id(&self) -> &str {
            "mock-hang"
        }

        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    // ── Provider that truncates a tool call on attempt 1, succeeds on 2 ───────
    // #1712: models the real failure — a native streaming tool call cut off by
    // the output cap (Done(MaxTokens) + unterminated args) on the first attempt,
    // then a clean call on the retry. Records the `max_tokens` it saw each call
    // so the test can assert the retry was issued with a RAISED budget.

    struct TruncateThenSucceedProvider {
        counters: Arc<CallCounters>,
        seen_max_tokens: Arc<std::sync::Mutex<Vec<Option<u32>>>>,
    }

    #[async_trait]
    impl LlmProvider for TruncateThenSucceedProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> eyre::Result<ChatResponse> {
            self.counters.chat.fetch_add(1, Ordering::SeqCst);
            eyre::bail!("non-streaming fallback should not be reached in this test")
        }

        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            config: &ChatConfig,
        ) -> eyre::Result<ChatStream> {
            let n = self.counters.chat_stream.fetch_add(1, Ordering::SeqCst);
            self.seen_max_tokens.lock().unwrap().push(config.max_tokens);
            if n == 0 {
                // Attempt 1: truncated mid-args, finished on the output cap.
                let events = vec![
                    StreamEvent::ToolCallDelta {
                        index: 0,
                        id: Some("write_file_26".to_string()),
                        name: Some("write_file".to_string()),
                        arguments_delta: "{\"path\":\"r.md\",\"content\":\"# Rev".to_string(),
                    },
                    StreamEvent::Usage(LlmTokenUsage::default()),
                    StreamEvent::Done(StopReason::MaxTokens),
                ];
                Ok(Box::pin(stream::iter(events)))
            } else {
                // Attempt 2 (bumped budget): a clean, complete tool call.
                let events = vec![
                    StreamEvent::ToolCallDelta {
                        index: 0,
                        id: Some("write_file_27".to_string()),
                        name: Some("write_file".to_string()),
                        arguments_delta: "{\"path\":\"r.md\",\"content\":\"# Review\"}".to_string(),
                    },
                    StreamEvent::Usage(LlmTokenUsage::default()),
                    StreamEvent::Done(StopReason::ToolUse),
                ];
                Ok(Box::pin(stream::iter(events)))
            }
        }

        fn model_id(&self) -> &str {
            // Resolves to a large max_output_tokens via context::max_output_tokens.
            "minimax-m3"
        }

        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    // ── Test helpers ──────────────────────────────────────────────────────────

    async fn build_agent(provider: Arc<dyn LlmProvider>) -> (Agent, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
        let tools = ToolRegistry::new();
        let agent = Agent::new(AgentId::new("llm-call-test"), provider, tools, memory);
        (agent, dir)
    }

    fn msgs() -> Vec<Message> {
        vec![Message::user("hello")]
    }

    fn turn() -> LoopTurnState {
        LoopTurnState::new(Instant::now())
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    /// Under FailFast, a provider whose stream always errors (retryable 503):
    ///   - chat_stream called exactly once (no retries)
    ///   - chat (non-streaming fallback) never called
    #[tokio::test]
    async fn should_call_once_and_skip_fallback_when_failfast_stream_error() {
        let counters = Arc::new(CallCounters::default());
        let provider = Arc::new(AlwaysErrStreamProvider {
            counters: counters.clone(),
        });
        let (agent, _dir) = build_agent(provider).await;

        let result = with_llm_call_policy(LlmCallPolicy::FailFast, async {
            agent
                .call_llm_with_hooks(
                    &msgs(),
                    &[],
                    &ChatConfig::default(),
                    1,
                    &octos_core::TokenUsage::default(),
                    &mut turn(),
                )
                .await
        })
        .await;

        assert!(result.is_err(), "should propagate the stream error");
        assert_eq!(
            counters.chat_stream.load(Ordering::SeqCst),
            1,
            "chat_stream must be called exactly once under FailFast"
        );
        assert_eq!(
            counters.chat.load(Ordering::SeqCst),
            0,
            "non-streaming fallback must NOT be called under FailFast"
        );
    }

    /// #1712: a truncated tool call (Done(MaxTokens) + unterminated args) is
    /// RETRYABLE — the loop retries (not instant death) AND raises the output
    /// budget for the retry so it can complete. Asserts: two stream attempts,
    /// the second issued with a bumped max_tokens, and the call ultimately
    /// succeeds with the completed tool call.
    #[tokio::test]
    async fn truncated_tool_call_retries_with_raised_output_budget() {
        let counters = Arc::new(CallCounters::default());
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(TruncateThenSucceedProvider {
            counters: counters.clone(),
            seen_max_tokens: seen.clone(),
        });
        let (agent, _dir) = build_agent(provider).await;

        // Start with a small per-turn cap (what cuts the call off).
        let small_cap = 1200u32;
        let config = ChatConfig {
            max_tokens: Some(small_cap),
            ..Default::default()
        };

        let result = agent
            .call_llm_with_hooks(
                &msgs(),
                &[],
                &config,
                1,
                &octos_core::TokenUsage::default(),
                &mut turn(),
            )
            .await;

        let (response, _streamed, _cost) = result.expect("truncation must recover, not fail");
        assert_eq!(
            counters.chat_stream.load(Ordering::SeqCst),
            2,
            "must retry once after the truncated call"
        );
        assert_eq!(
            response.tool_calls.len(),
            1,
            "the retry's completed tool call must surface"
        );
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "two stream attempts");
        assert_eq!(seen[0], Some(small_cap), "attempt 1 uses the small cap");
        assert!(
            seen[1].unwrap() > small_cap,
            "attempt 2 must raise the output budget (was {:?})",
            seen[1]
        );
    }

    /// Under FailFast, a provider whose stream always returns an empty response:
    ///   - chat_stream called exactly once (no retries)
    ///   - chat (non-streaming fallback) never called
    #[tokio::test]
    async fn should_call_once_and_skip_fallback_when_failfast_empty_response() {
        let counters = Arc::new(CallCounters::default());
        let provider = Arc::new(AlwaysEmptyProvider {
            counters: counters.clone(),
        });
        let (agent, _dir) = build_agent(provider).await;

        let result = with_llm_call_policy(LlmCallPolicy::FailFast, async {
            agent
                .call_llm_with_hooks(
                    &msgs(),
                    &[],
                    &ChatConfig::default(),
                    1,
                    &octos_core::TokenUsage::default(),
                    &mut turn(),
                )
                .await
        })
        .await;

        assert!(result.is_err(), "should propagate the empty response error");
        assert_eq!(
            counters.chat_stream.load(Ordering::SeqCst),
            1,
            "chat_stream must be called exactly once under FailFast (empty response)"
        );
        assert_eq!(
            counters.chat.load(Ordering::SeqCst),
            0,
            "non-streaming fallback must NOT be called under FailFast (empty response)"
        );
    }

    /// Under FailFast, a provider that hangs forever at stream *build* must be
    /// bounded by the voice overall deadline (the `StreamTimeouts` guards only
    /// start once `consume_stream` runs, so they cannot catch a build-phase
    /// hang). The call returns `Err` well within the deadline + slack.
    #[tokio::test]
    async fn should_timeout_build_stream_when_failfast() {
        let (agent, _dir) = build_agent(Arc::new(HangingBuildProvider)).await;
        let agent = agent.with_config(AgentConfig {
            voice_overall_deadline: Duration::from_millis(50),
            ..AgentConfig::default()
        });

        let start = Instant::now();
        let result = with_llm_call_policy(LlmCallPolicy::FailFast, async {
            agent
                .call_llm_with_hooks(
                    &msgs(),
                    &[],
                    &ChatConfig::default(),
                    1,
                    &octos_core::TokenUsage::default(),
                    &mut turn(),
                )
                .await
        })
        .await;

        assert!(
            result.is_err(),
            "build-stream hang must surface as an error"
        );
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "build-stream hang must be bounded by the voice overall deadline, took {:?}",
            start.elapsed()
        );
    }

    /// Normal policy keeps the long production backstop — the voice deadline is
    /// not applied — so the same hang is NOT bounded by 50ms. (We only assert
    /// it does not return quickly; we never actually wait out the real cap.)
    #[tokio::test]
    async fn should_not_apply_voice_deadline_when_normal_policy() {
        let (agent, _dir) = build_agent(Arc::new(HangingBuildProvider)).await;
        let agent = agent.with_config(AgentConfig {
            voice_overall_deadline: Duration::from_millis(50),
            ..AgentConfig::default()
        });

        // Under Normal policy the 50ms voice deadline must be ignored, so the
        // call is still pending after comfortably more than 50ms.
        let pending = tokio::time::timeout(Duration::from_millis(300), async {
            agent
                .call_llm_with_hooks(
                    &msgs(),
                    &[],
                    &ChatConfig::default(),
                    1,
                    &octos_core::TokenUsage::default(),
                    &mut turn(),
                )
                .await
        })
        .await;
        assert!(
            pending.is_err(),
            "Normal policy must NOT apply the 50ms voice deadline"
        );
    }
}
