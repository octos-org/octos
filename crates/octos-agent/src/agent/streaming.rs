//! Stream consumption, shutdown handling, and cost reporting.

use std::sync::atomic::{AtomicU64, Ordering};

use eyre::Result;
use futures::StreamExt;
use octos_core::{Message, MessageRole, TokenUsage};
use octos_llm::{ChatResponse, ChatStream, StopReason, StreamError, StreamEvent};
use tracing::warn;

use super::Agent;
use crate::progress::ProgressEvent;

/// Process-global monotonic counter for synthesizing tool-call ids when a
/// provider streams a tool call with no `id`. MUST be globally unique (not a
/// per-response positional index): `TaskSupervisor`'s
/// `synth_ack_emitted_tool_call_ids` set is long-lived per session, so a
/// positional id reused across responses could match a stale ack and fire an
/// unwarranted recovery turn (codex P2).
static SYNTH_TOOL_CALL_SEQ: AtomicU64 = AtomicU64::new(0);

/// Default inter-chunk idle timeout for SSE streams.
///
/// Codex round (PR #1355) bumped this from 30s → 180s based on production
/// evidence (mini3 2026-05-28 mofa_slides failure batch): kimi-k2.5/autodl,
/// claude-opus thinking blocks, and other reasoning-heavy models legitimately
/// pause for minutes between chunks. The 30s default produced false-positive
/// stalls under normal load. codex's reference value is 5 minutes; 180s is a
/// middle-ground that keeps a genuinely-stuck call from blocking forever.
///
/// See `docs/STREAMING-TRANSACTIONAL-BOUNDARY-ADR.md`.
pub(super) const STREAM_INTER_CHUNK_IDLE_TIMEOUT_SECS: u64 = 180;

impl Agent {
    /// Wait until the shutdown flag is set. Used with `tokio::select!`
    /// to cancel long-running operations on Ctrl+C.
    ///
    /// Codex round (PR #1355): the previous implementation returned after
    /// 30 seconds even when no shutdown signal arrived ("30s safety
    /// guard"). That deadline raced the inter-chunk stream timeout: with
    /// the 180s timeout for reasoning models, the safety guard fired
    /// first and broke the SSE loop with a false "shutdown received"
    /// signal — exactly the silent-state-shipping symptom this PR is
    /// trying to eliminate. The safety guard had no production user
    /// (Ctrl+C sets the atomic; no other consumer relied on the 30s
    /// return) so the deadline is removed; the function now polls the
    /// atomic until the flag flips.
    pub(super) async fn wait_for_shutdown(&self) {
        loop {
            if self.shutdown.load(Ordering::Acquire) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    pub(super) async fn consume_stream_with_input_estimate(
        &self,
        stream: ChatStream,
        iteration: u32,
        input_tokens_estimate: u32,
    ) -> Result<(ChatResponse, bool)> {
        self.consume_stream_inner(
            stream,
            iteration,
            input_tokens_estimate,
            STREAM_INTER_CHUNK_IDLE_TIMEOUT_SECS,
        )
        .await
    }

    /// Test-only entry point: lets fixtures dial the inter-chunk idle
    /// timeout down to milliseconds so virtual-time orchestration
    /// (`#[tokio::test(start_paused)]`) can race the 30s `wait_for_shutdown`
    /// safety guard cleanly. Production callers always use the
    /// 180s constant via `consume_stream_with_input_estimate`.
    #[cfg(test)]
    pub(super) async fn consume_stream_for_test(
        &self,
        stream: ChatStream,
        iteration: u32,
        input_tokens_estimate: u32,
        inter_chunk_idle_secs: u64,
    ) -> Result<(ChatResponse, bool)> {
        self.consume_stream_inner(
            stream,
            iteration,
            input_tokens_estimate,
            inter_chunk_idle_secs,
        )
        .await
    }

    async fn consume_stream_inner(
        &self,
        mut stream: ChatStream,
        iteration: u32,
        input_tokens_estimate: u32,
        // Codex round (PR #1355): the inter-chunk idle timeout is passed in
        // explicitly so test fixtures can dial it down to milliseconds
        // (the production default is 180s, see
        // `STREAM_INTER_CHUNK_IDLE_TIMEOUT_SECS`). Production callers go
        // through `consume_stream_with_input_estimate` which always uses
        // the constant.
        inter_chunk_idle_secs: u64,
    ) -> Result<(ChatResponse, bool)> {
        // Clear any pending status line (e.g., "Thinking...")
        self.reporter().report(ProgressEvent::Response {
            content: String::new(),
            iteration,
        });

        let mut text = String::new();
        let mut reasoning = String::new();
        // (id, name, args_json, metadata)
        let mut tool_calls: Vec<(String, String, String, Option<serde_json::Value>)> = Vec::new();
        let mut usage = octos_llm::TokenUsage::default();
        let mut stop_reason = StopReason::EndTurn;
        let mut provider_index = None;

        // Adaptive stream timeout:
        // - TTFT (first token): generous — models need time to process large
        //   inputs before generating. Scales with input: base 30s + 1s per 1K
        //   input tokens, capped at 180s.
        // - Inter-chunk: once streaming starts, codex round PR #1355 bumped
        //   from 30s → STREAM_INTER_CHUNK_IDLE_TIMEOUT_SECS (180s) to give
        //   reasoning models legitimate room to pause. The 30s value was
        //   producing false-positive stalls on kimi-k2.5/autodl mofa_slides
        //   calls; production evidence in
        //   `docs/STREAMING-TRANSACTIONAL-BOUNDARY-ADR.md`.
        let ttft_secs = (30 + input_tokens_estimate as u64 / 1000).min(180);
        let mut got_first_chunk = false;
        // Codex round (PR #1355): track whether we observed an explicit
        // `Done` event. Combined with the tool_calls list this lets us
        // distinguish a real completion from a stream that dropped its
        // terminal signal before delivering all the tool_call args bytes —
        // the latter used to be silently fixed up via "fixing stop_reason"
        // and shipped downstream; now it returns `StreamError::Incomplete`.
        let mut saw_done = false;

        loop {
            let timeout = if got_first_chunk {
                std::time::Duration::from_secs(inter_chunk_idle_secs)
            } else {
                std::time::Duration::from_secs(ttft_secs)
            };

            let event = tokio::select! {
                event = stream.next() => event,
                _ = self.wait_for_shutdown() => {
                    warn!("shutdown received during streaming");
                    break;
                }
                _ = tokio::time::sleep(timeout) => {
                    // Codex round (PR #1355): inter-chunk timeout used to
                    // silently `break` with a half-assembled
                    // `tool_call.arguments` buffer; that buffer was then
                    // wrapped as a `MALFORMED_JSON:` sentinel and shipped
                    // downstream as if it were a valid ChatResponse. The
                    // plugin executor dispatched the sentinel string and
                    // errored with "missing 'out'". The structural fix:
                    // return a typed `StreamError::IdleTimeout`. The
                    // existing `RetryProvider` / `ProviderChain` machinery
                    // upstream surfaces this via `is_retryable_stream_error`
                    // (matches "stream idle timeout") and retries. No
                    // partial state ever leaves this function.
                    let idle_secs = if got_first_chunk {
                        inter_chunk_idle_secs
                    } else {
                        ttft_secs
                    };
                    if got_first_chunk {
                        warn!(
                            "stream inter-chunk timeout after {idle_secs}s — provider stalled"
                        );
                    } else {
                        warn!(
                            "stream TTFT timeout after {ttft_secs}s (input_estimate={input_tokens_estimate})"
                        );
                    }
                    return Err(eyre::Report::new(StreamError::IdleTimeout { idle_secs }));
                }
            };

            let Some(event) = event else {
                tracing::debug!("stream ended (None)");
                break;
            };
            tracing::debug!(?event, "stream event received");

            match event {
                StreamEvent::ProviderIndex(index) => {
                    provider_index = Some(index);
                }
                StreamEvent::ReasoningDelta(delta) => {
                    got_first_chunk = true;
                    reasoning.push_str(&delta);
                }
                StreamEvent::TextDelta(delta) => {
                    got_first_chunk = true;
                    self.reporter().report(ProgressEvent::StreamChunk {
                        text: delta.clone(),
                        iteration,
                    });
                    text.push_str(&delta);
                }
                StreamEvent::ToolCallDelta {
                    index,
                    id,
                    name,
                    arguments_delta,
                } => {
                    // Codex round (PR #1355): tool_call deltas count as
                    // "first chunk" for the TTFT → inter-chunk timeout
                    // transition. Without this a tool_call-only response
                    // (no text) would stay on the 30s TTFT timeout
                    // forever — the second poll uses TTFT instead of the
                    // inter-chunk budget, so a model that legitimately
                    // streams tool args slowly gets falsely flagged as a
                    // TTFT stall. Both `ReasoningDelta` and `TextDelta`
                    // already toggle this; `ToolCallDelta` should too.
                    got_first_chunk = true;
                    while tool_calls.len() <= index {
                        tool_calls.push((String::new(), String::new(), String::new(), None));
                    }
                    if let Some(id) = id {
                        tool_calls[index].0 = id;
                    }
                    if let Some(name) = name {
                        tool_calls[index].1 = name;
                    }
                    tool_calls[index].2.push_str(&arguments_delta);
                }
                StreamEvent::ToolCallMetadata { index, metadata } => {
                    while tool_calls.len() <= index {
                        tool_calls.push((String::new(), String::new(), String::new(), None));
                    }
                    tool_calls[index].3 = Some(metadata);
                }
                StreamEvent::Usage(u) => {
                    usage = u;
                }
                StreamEvent::Done(reason) => {
                    stop_reason = reason;
                    saw_done = true;
                }
                StreamEvent::Error(err) => {
                    // Codex round (PR #1355): surface as typed Transport
                    // error so `is_retryable_stream_error` / `LlmError`
                    // bridge can route it through the existing retry
                    // ladder. The previous `eyre::bail!` carried the same
                    // semantics but without a typed downcast path.
                    return Err(eyre::Report::new(StreamError::Transport { detail: err }));
                }
            }
        }

        let streamed = !text.is_empty();
        if streamed {
            self.reporter()
                .report(ProgressEvent::StreamDone { iteration });
        }

        // Strip <think> tags from accumulated streaming content (some models
        // embed chain-of-thought in <think> tags via TextDelta instead of
        // using ReasoningDelta events).
        let (text, think_extracted) = octos_llm::strip_think_tags(&text);
        if let Some(ref extracted) = think_extracted {
            if reasoning.is_empty() {
                reasoning = extracted.clone();
            }
        }

        let content = if text.is_empty() { None } else { Some(text) };
        // Codex round (PR #1355): parse tool_call arguments strictly. The
        // previous code fell back to a `Value::String("MALFORMED_JSON:...")`
        // sentinel when JSON parsing failed, which then shipped downstream
        // as if it were a valid ChatResponse — the plugin executor
        // dispatched the sentinel string and errored "missing 'out'". The
        // new contract: a clean stream (saw_done == true) with garbage in
        // args is a model-side bug → `StreamError::MalformedArgs`
        // (non-retryable; the model needs to see the diagnostic). An
        // incomplete stream's parse failure cannot reach this branch
        // because the `IdleTimeout` / `Transport` paths above already
        // short-circuited.
        //
        // The `write_file`-specific `recover_write_file_args` salvager and
        // its `extract_json_string_field` helper are deleted in the same
        // PR — with the boundary in place they were treating symptoms of
        // the missing invariant, not addressing it.
        let mut parsed_tool_calls: Vec<octos_core::ToolCall> = Vec::with_capacity(tool_calls.len());
        for (id, name, args, metadata) in tool_calls.into_iter() {
            if name.is_empty() {
                continue;
            }
            // Some OpenAI-compatible providers (kimi / MiniMax via wisemodel)
            // stream tool calls with no `id`. An empty tool_call_id is not
            // cosmetic: it silently disables spawn_only failure-recovery
            // downstream — `TaskSupervisor::notify_failure` returns early on
            // an empty id (it can't key the synth-ack lookup), so a failed
            // background skill never routes a recovery turn back to the LLM
            // and the model can't fix its own bad input. Mint a PROCESS-UNIQUE
            // id (monotonic counter, NOT a positional `call_{index}`): the
            // supervisor's synth-ack set is long-lived per session, so a
            // positional id reused across responses could match a stale ack
            // and fire an unwarranted recovery turn (codex P2).
            let id = if id.is_empty() {
                format!(
                    "call_synth_{}",
                    SYNTH_TOOL_CALL_SEQ.fetch_add(1, Ordering::Relaxed)
                )
            } else {
                id
            };
            match serde_json::from_str(&args) {
                Ok(arguments) => parsed_tool_calls.push(octos_core::ToolCall {
                    id,
                    name,
                    arguments,
                    metadata,
                }),
                Err(e) => {
                    let truncated_raw = octos_core::truncated_utf8(&args, 200, "...");
                    tracing::warn!(
                        tool = %name,
                        tool_id = %id,
                        error = %e,
                        raw = %truncated_raw,
                        "malformed tool call JSON — surfacing as StreamError::MalformedArgs"
                    );
                    return Err(eyre::Report::new(StreamError::MalformedArgs {
                        tool_id: id,
                        tool_name: name,
                        error: format!("{e} (raw: {truncated_raw})"),
                    }));
                }
            }
        }
        let tool_calls = parsed_tool_calls;

        let reasoning_content = if reasoning.is_empty() {
            None
        } else {
            Some(reasoning)
        };

        // Codex round (PR #1355): the old code silently coerced
        // `EndTurn + tool_calls` → `ToolUse` and emitted a "fixing
        // stop_reason" warning. That was masking provider weirdness AND
        // streaming-layer incompleteness (a stream that dropped its
        // terminal `Done` event before delivering all tool_call args
        // would default to `StopReason::EndTurn` and get fixed up). The
        // new contract: this combination is `StreamError::Incomplete` —
        // a typed retryable error the existing retry/failover ladder can
        // route. The `saw_done` flag is part of the boundary: when a
        // provider does emit `Done(ToolUse)` properly the parsing path
        // above sets `stop_reason` correctly and we never enter this
        // branch.
        if !tool_calls.is_empty() && stop_reason == StopReason::EndTurn {
            return Err(eyre::Report::new(StreamError::Incomplete {
                detail: format!(
                    "stream produced {} tool_call(s) but stop_reason is EndTurn (saw_done={saw_done})",
                    tool_calls.len()
                ),
            }));
        }

        // Detect repetitive/looping output -- model got stuck repeating itself.
        // Replace with a short message so the user sees something useful.
        let content = if let Some(ref text) = content {
            if Self::is_repetitive_output(text) {
                tracing::warn!(
                    content_len = text.len(),
                    "detected repetitive LLM output, replacing with error message"
                );
                None
            } else {
                content
            }
        } else {
            content
        };

        Ok((
            ChatResponse {
                content,
                reasoning_content,
                tool_calls,
                stop_reason,
                usage,
                provider_index,
            },
            streamed,
        ))
    }

    pub(super) fn emit_cost_update(&self, total_usage: &TokenUsage, response: &ChatResponse) {
        let response_usage = &response.usage;
        // Codex round-1 P2: for failover / routed responses the slot that
        // produced this response may not match `self.llm.model_id()`
        // (which exposes the chain's "active" slot). Resolve via
        // `provider_metadata_for_index` so the footer reflects the model
        // that actually answered. `provider_metadata_for_index` falls
        // back to the active slot's metadata when `provider_index` is
        // `None`, matching the legacy `model_id()` behaviour.
        let metadata = self
            .llm
            .provider_metadata_for_index(response.provider_index);
        let pricing = octos_llm::pricing::model_pricing(&metadata.model);
        let response_cost =
            pricing.map(|p| p.cost(response_usage.input_tokens, response_usage.output_tokens));
        let session_cost =
            pricing.map(|p| p.cost(total_usage.input_tokens, total_usage.output_tokens));
        // Carry the model id so chat clients can render
        // `model · tokens_in / tokens_out · duration` footers. Skip the
        // synthesis if the provider returns an empty identifier — empty
        // strings would only confuse the client renderer.
        let model = if metadata.model.is_empty() {
            None
        } else {
            Some(metadata.model.clone())
        };
        self.reporter().report(ProgressEvent::CostUpdate {
            session_input_tokens: total_usage.input_tokens,
            session_output_tokens: total_usage.output_tokens,
            response_cost,
            session_cost,
            model,
        });
    }

    pub(super) fn response_to_message(&self, response: &ChatResponse) -> Message {
        Message {
            role: MessageRole::Assistant,
            content: response.content.clone().unwrap_or_default(),
            media: vec![],
            tool_calls: if response.tool_calls.is_empty() {
                None
            } else {
                Some(response.tool_calls.clone())
            },
            tool_call_id: None,
            reasoning_content: response.reasoning_content.clone(),
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        }
    }
}

// Codex round (PR #1355): `recover_write_file_args` and its helper
// `extract_json_string_field` previously lived here. They were
// tool-specific salvagers that compensated for the missing streaming
// transactional boundary — when SSE timeouts produced half-assembled
// `write_file` args buffers, the salvager scraped what it could and
// shipped a "truncated but better than lost" file write downstream.
// With the boundary in place (typed `StreamError::IdleTimeout` /
// `Incomplete` / `MalformedArgs`), the retry machinery handles these
// transparently and a `write_file` call with bad args becomes a typed
// model-facing error rather than a silent partial write. The salvagers
// were deleted in the same PR.
//
// See `docs/STREAMING-TRANSACTIONAL-BOUNDARY-ADR.md` for the full
// rationale.

#[cfg(test)]
mod tests {
    //! Streaming transactional-boundary tests (PR #1355).
    //!
    //! Each test feeds a synthetic `ChatStream` into `consume_stream_inner`
    //! and asserts the typed `StreamError` contract:
    //!
    //! * Idle timeout → `Err(IdleTimeout)`; no partial buffer surfaces.
    //! * Done(EndTurn) + tool_calls → `Err(Incomplete)`; the legacy
    //!   silent fix-up to ToolUse is gone.
    //! * Done(ToolUse) + malformed args → `Err(MalformedArgs)`; the
    //!   `MALFORMED_JSON:` sentinel is gone.
    //! * Done(EndTurn) with text only → `Ok` happy path (regression).
    //! * Done(ToolUse) with valid args → `Ok` happy path (regression).
    //!
    //! These tests run with `tokio::time::pause` so the 180s inter-chunk
    //! timeout fires instantly under virtual time.

    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use eyre::Result;
    use futures::StreamExt;
    use futures::stream;
    use octos_core::{AgentId, Message};
    use octos_llm::{
        ChatConfig, ChatResponse, ChatStream, LlmProvider, StopReason, StreamError, StreamEvent,
        TokenUsage as LlmTokenUsage, ToolSpec,
    };
    use octos_memory::EpisodeStore;
    use serde_json::json;
    use tempfile::TempDir;

    use super::super::Agent;
    use crate::tools::ToolRegistry;

    struct NoopProvider;

    #[async_trait]
    impl LlmProvider for NoopProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> Result<ChatResponse> {
            eyre::bail!("chat() unused in streaming tests")
        }

        fn model_id(&self) -> &str {
            "mock"
        }

        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    /// Build a bare `Agent` whose backing provider is unused — the streaming
    /// tests drive `consume_stream_inner` with hand-built streams.
    async fn build_test_agent() -> (Agent, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
        let provider: Arc<dyn LlmProvider> = Arc::new(NoopProvider);
        let tools = ToolRegistry::new();
        let agent = Agent::new(AgentId::new("stream-test"), provider, tools, memory);
        (agent, dir)
    }

    fn into_chat_stream(events: Vec<StreamEvent>) -> ChatStream {
        Box::pin(stream::iter(events))
    }

    /// A stream that yields one event then stalls forever — used to assert
    /// the inter-chunk idle timeout fires and returns
    /// `StreamError::IdleTimeout` rather than shipping the partial buffer.
    fn stalling_stream(prelude: Vec<StreamEvent>) -> ChatStream {
        let stalled = stream::iter(prelude).chain(stream::pending::<StreamEvent>());
        Box::pin(stalled)
    }

    /// Downcast an `eyre::Report` to `StreamError`, returning `None` when
    /// the error is not the typed variant.
    fn as_stream_error(err: &eyre::Report) -> Option<&StreamError> {
        err.downcast_ref::<StreamError>()
    }

    #[tokio::test]
    async fn stream_idle_timeout_returns_err_not_partial_buffer() {
        // PR #1355: the previous code would silently `break` here and ship
        // the half-assembled `tool_call.arguments` buffer downstream as a
        // sentinel. The new contract: typed `StreamError::IdleTimeout`,
        // no `Ok` ever produced with partial state.
        //
        // The test uses real time with a 1s idle timeout (vs production's
        // 180s) so the timeout fires before the 30s `wait_for_shutdown`
        // safety-guard deadline. Virtual-time orchestration is avoided
        // because it would auto-advance past the safety guard and break
        // the loop on the shutdown branch instead of the timeout branch.
        let (agent, _dir) = build_test_agent().await;

        // Half-emit a tool_call: open the JSON object but never close it.
        // Then stall — the inter-chunk timeout must fire before the SSE
        // loop ever sees a Done event.
        let stream = stalling_stream(vec![StreamEvent::ToolCallDelta {
            index: 0,
            id: Some("call_0".to_string()),
            name: Some("mofa_slides".to_string()),
            arguments_delta: "{\"style\": \"sun\", \"slides\": [\"intro".to_string(),
        }]);

        let start = std::time::Instant::now();
        // input_tokens_estimate = 0 forces ttft to its minimum (30s in the
        // formula `30 + estimate/1000`). For the test we want the stream
        // arm to win the first iteration's `select!` so we transition to
        // `got_first_chunk = true` immediately and the second iteration
        // uses our 1s `inter_chunk_idle_secs`. Real time + 1s vs 30s ttft
        // is fine — the stream::iter event resolves in micros.
        let result = agent.consume_stream_for_test(stream, 1, 0, 1).await;
        let elapsed = start.elapsed();

        let err = result.expect_err("idle timeout must surface as Err");
        let typed = as_stream_error(&err).expect("err must be StreamError typed");
        assert!(
            matches!(typed, StreamError::IdleTimeout { .. }),
            "expected IdleTimeout, got {typed:?} (elapsed={elapsed:?})"
        );
        assert!(
            typed.is_retryable(),
            "idle timeout must be retryable so RetryProvider drives recovery"
        );
        // The 1s idle timeout MUST fire before the 30s `wait_for_shutdown`
        // safety guard. If the test takes >= 25s the safety guard fired
        // first and the test result is by coincidence — fix the
        // production code so `wait_for_shutdown` cannot break the stream
        // loop with a false shutdown signal.
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "idle timeout took {elapsed:?} — wait_for_shutdown safety guard likely fired \
             before the 1s timeout, which means the test passes by accident"
        );
    }

    #[tokio::test]
    async fn stream_synthesizes_tool_call_id_when_provider_omits_it() {
        // Regression (mofa_slides slides-1780072199773-2htqt1, mini3,
        // 2026-05-29): kimi / MiniMax via wisemodel stream tool calls with
        // NO `id` field. An empty tool_call_id silently disables spawn_only
        // failure-recovery downstream — `TaskSupervisor::notify_failure`
        // returns early on an empty id (it can't key the synth-ack lookup),
        // so a failed background skill never routes a recovery turn back to
        // the LLM and the model can't fix its own bad input. The streaming
        // assembler must mint a stable, unique positional id (matching the
        // Gemini provider's `call_{n}` convention) so a non-empty id always
        // reaches the agent loop.
        let (agent, _dir) = build_test_agent().await;

        let stream = into_chat_stream(vec![
            StreamEvent::ToolCallDelta {
                index: 0,
                id: None,
                name: Some("mofa_slides".to_string()),
                arguments_delta: r#"{"deck":"x"}"#.to_string(),
            },
            StreamEvent::ToolCallDelta {
                index: 1,
                id: None,
                name: Some("glob".to_string()),
                arguments_delta: r#"{"pattern":"*.toml"}"#.to_string(),
            },
            StreamEvent::Usage(LlmTokenUsage::default()),
            StreamEvent::Done(StopReason::ToolUse),
        ]);

        let (resp, _streamed) = agent
            .consume_stream_with_input_estimate(stream, 1, 100)
            .await
            .expect("clean tool-use stream must assemble into a ChatResponse");

        assert_eq!(resp.tool_calls.len(), 2);
        assert!(
            resp.tool_calls
                .iter()
                .all(|tc| tc.id.starts_with("call_synth_")),
            "empty provider tool_call ids must be synthesized, not passed through: {:?}",
            resp.tool_calls.iter().map(|tc| &tc.id).collect::<Vec<_>>()
        );
        assert_ne!(
            resp.tool_calls[0].id, resp.tool_calls[1].id,
            "synthesized ids must be unique per tool call"
        );

        // codex P2: ids must be unique ACROSS responses too — the supervisor's
        // synth-ack set is long-lived per session, so a positional `call_0`
        // reused on a later turn could match a stale ack. A second assembly
        // must produce a disjoint id set.
        let first_ids: std::collections::HashSet<String> =
            resp.tool_calls.iter().map(|tc| tc.id.clone()).collect();
        let stream2 = into_chat_stream(vec![
            StreamEvent::ToolCallDelta {
                index: 0,
                id: None,
                name: Some("mofa_slides".to_string()),
                arguments_delta: r#"{"deck":"y"}"#.to_string(),
            },
            StreamEvent::Usage(LlmTokenUsage::default()),
            StreamEvent::Done(StopReason::ToolUse),
        ]);
        let (resp2, _) = agent
            .consume_stream_with_input_estimate(stream2, 2, 100)
            .await
            .expect("second stream must assemble");
        assert!(
            resp2
                .tool_calls
                .iter()
                .all(|tc| !first_ids.contains(&tc.id)),
            "synthesized ids must not collide across responses (codex P2): first={first_ids:?} second={:?}",
            resp2.tool_calls.iter().map(|tc| &tc.id).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn stream_malformed_args_returns_err() {
        // PR #1355: when a stream produces tool_calls + Done(ToolUse) but
        // the assembled args buffer fails to parse as JSON, the contract
        // is `StreamError::MalformedArgs` — NOT the legacy
        // `Value::String("MALFORMED_JSON:...")` sentinel that used to ship
        // downstream as if it were a valid argument.
        let (agent, _dir) = build_test_agent().await;

        let stream = into_chat_stream(vec![
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call_0".to_string()),
                name: Some("mofa_slides".to_string()),
                arguments_delta: "this is not json at all".to_string(),
            },
            StreamEvent::Usage(LlmTokenUsage::default()),
            StreamEvent::Done(StopReason::ToolUse),
        ]);

        let result = agent
            .consume_stream_with_input_estimate(stream, 1, 100)
            .await;

        let err = result.expect_err("malformed args must surface as Err");
        let typed = as_stream_error(&err).expect("err must be StreamError typed");
        match typed {
            StreamError::MalformedArgs {
                tool_id, tool_name, ..
            } => {
                assert_eq!(tool_id, "call_0");
                assert_eq!(tool_name, "mofa_slides");
            }
            other => panic!("expected MalformedArgs, got {other:?}"),
        }
        assert!(
            !typed.is_retryable(),
            "MalformedArgs must NOT be retryable — the model needs to see the diagnostic"
        );
    }

    #[tokio::test]
    async fn stream_endturn_with_toolcalls_returns_incomplete() {
        // PR #1355: the old code coerced `EndTurn + tool_calls` → `ToolUse`
        // with a "fixing stop_reason" warning. That was masking
        // streaming-layer incompleteness (a stream that dropped its
        // terminal Done event would default to EndTurn). The new
        // contract: typed `StreamError::Incomplete`.
        let (agent, _dir) = build_test_agent().await;

        let stream = into_chat_stream(vec![
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call_0".to_string()),
                name: Some("shell".to_string()),
                arguments_delta: "{\"cmd\": \"ls\"}".to_string(),
            },
            // Stream ends without a Done event — `stop_reason` stays at
            // its EndTurn default. Pre-PR-1355 this was silently fixed
            // up to `ToolUse`; now it's `StreamError::Incomplete`.
        ]);

        let result = agent
            .consume_stream_with_input_estimate(stream, 1, 100)
            .await;

        let err = result.expect_err("EndTurn + tool_calls must surface as Err");
        let typed = as_stream_error(&err).expect("err must be StreamError typed");
        assert!(matches!(typed, StreamError::Incomplete { .. }));
        assert!(
            typed.is_retryable(),
            "Incomplete must be retryable so the lane router can pick a different slot"
        );
    }

    #[tokio::test]
    async fn stream_complete_with_tool_use_returns_chat_response() {
        // Happy path regression: a clean stream with valid tool_call args
        // and a Done(ToolUse) signal returns Ok(ChatResponse) with the
        // arguments parsed as a Value::Object.
        let (agent, _dir) = build_test_agent().await;

        let stream = into_chat_stream(vec![
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call_0".to_string()),
                name: Some("shell".to_string()),
                arguments_delta: "{\"cmd\":\"ls\"}".to_string(),
            },
            StreamEvent::Usage(LlmTokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            }),
            StreamEvent::Done(StopReason::ToolUse),
        ]);

        let (response, streamed) = agent
            .consume_stream_with_input_estimate(stream, 1, 100)
            .await
            .expect("clean stream must return Ok");
        assert!(!streamed, "stream had no text deltas");
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "shell");
        assert_eq!(response.tool_calls[0].arguments, json!({"cmd": "ls"}));
        assert_eq!(response.stop_reason, StopReason::ToolUse);
    }

    #[tokio::test]
    async fn stream_complete_with_text_returns_chat_response() {
        // Happy path regression #2: text-only assistant response with
        // Done(EndTurn) — no tool_calls — must still return Ok.
        let (agent, _dir) = build_test_agent().await;

        let stream = into_chat_stream(vec![
            StreamEvent::TextDelta("Hello".to_string()),
            StreamEvent::TextDelta(", world!".to_string()),
            StreamEvent::Usage(LlmTokenUsage::default()),
            StreamEvent::Done(StopReason::EndTurn),
        ]);

        let (response, streamed) = agent
            .consume_stream_with_input_estimate(stream, 1, 100)
            .await
            .expect("text-only stream must return Ok");
        assert!(streamed, "got text deltas → streamed=true");
        assert_eq!(response.content.as_deref(), Some("Hello, world!"));
        assert!(response.tool_calls.is_empty());
        assert_eq!(response.stop_reason, StopReason::EndTurn);
    }

    #[tokio::test]
    async fn stream_transport_error_returns_typed_err() {
        // Provider emits an explicit error event. We surface it as
        // `StreamError::Transport` so the typed retry policy can decide.
        let (agent, _dir) = build_test_agent().await;

        let stream = into_chat_stream(vec![
            StreamEvent::TextDelta("partial ".to_string()),
            StreamEvent::Error("connection reset by peer".to_string()),
        ]);

        let result = agent
            .consume_stream_with_input_estimate(stream, 1, 100)
            .await;
        let err = result.expect_err("stream error must surface as Err");
        let typed = as_stream_error(&err).expect("err must be StreamError typed");
        assert!(matches!(typed, StreamError::Transport { .. }));
        assert!(
            typed.is_retryable(),
            "transport errors should be retryable through the normal failover ladder"
        );
    }

    #[test]
    fn idle_timeout_constant_is_180s() {
        // Pin the constant value so a future tweak that brings it back
        // down to 30s (the production-broken value) trips this test.
        assert_eq!(
            super::STREAM_INTER_CHUNK_IDLE_TIMEOUT_SECS,
            180,
            "30s was producing false-positive stalls on reasoning models; \
             see docs/STREAMING-TRANSACTIONAL-BOUNDARY-ADR.md"
        );
        // Sanity: must be larger than legacy 30s value.
        let timeout_secs = super::STREAM_INTER_CHUNK_IDLE_TIMEOUT_SECS;
        assert!(timeout_secs > 30);
    }

    // Use Duration in a sanity check to make sure the constant is usable.
    #[test]
    fn idle_timeout_constant_builds_valid_duration() {
        let d = Duration::from_secs(super::STREAM_INTER_CHUNK_IDLE_TIMEOUT_SECS);
        assert_eq!(d.as_secs(), 180);
    }

    // Silence unused-import warnings in cfg(test) when one helper isn't used.
    #[test]
    fn _stream_helpers_compile() {
        let _ = into_chat_stream(vec![]);
        let _ = stalling_stream(vec![]);
    }
}
