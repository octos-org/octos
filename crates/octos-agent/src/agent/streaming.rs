//! Stream consumption, shutdown handling, and cost reporting.

use std::sync::atomic::{AtomicU64, Ordering};

use eyre::Result;
use futures::StreamExt;
use octos_core::{Message, MessageRole};
use octos_llm::{ChatResponse, ChatStream, StopReason, StreamError, StreamEvent};
use tracing::warn;

use super::Agent;
use super::turn_state::LoopTurnState;
use crate::progress::ProgressEvent;

/// Process-global monotonic counter for synthesizing tool-call ids when a
/// provider streams a tool call with no `id`. MUST be globally unique (not a
/// per-response positional index): `TaskSupervisor`'s
/// `synth_ack_emitted_tool_call_ids` set is long-lived per session, so a
/// positional id reused across responses could match a stale ack and fire an
/// unwarranted recovery turn (codex P2).
static SYNTH_TOOL_CALL_SEQ: AtomicU64 = AtomicU64::new(0);

/// Per-call streaming timeout budget. Bundles the three guards that bound a
/// single streaming LLM call so a stalled or pathologically-slow provider
/// cannot hang the turn forever:
///
/// * `first_token_grace_secs` — generous ceiling on time-to-first-token; a
///   reasoning model can take minutes before the first chunk.
/// * `inter_chunk_idle_secs` — once streaming starts, the max idle gap between
///   chunks; trips on a provider that stalls mid-flight.
/// * `overall_max_secs` — final wall-clock backstop measured from call start;
///   catches a stream that trickles a token every `<idle>` seconds forever.
///
/// Production values flow from `AgentConfig` (env-overridable). A value of `0`
/// for `overall_max_secs` disables the wall-clock backstop (idle/TTFT guards
/// still apply); the idle/TTFT fields are floored at 1s internally.
#[derive(Debug, Clone, Copy)]
pub(super) struct StreamTimeouts {
    pub first_token_grace_secs: u64,
    pub inter_chunk_idle_secs: u64,
    pub overall_max_secs: u64,
}

/// #1507: shown in place of suppressed repetitive output. The content must
/// never become `None` here — the `EndTurn` consumer renders
/// `content.unwrap_or_default()`, so `None` surfaced as a completely blank
/// assistant bubble with no hint that anything was suppressed.
pub(super) const REPETITIVE_OUTPUT_MESSAGE: &str = "The model got stuck repeating itself, so the repetitive output was suppressed. Please try again or rephrase the request.";

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
        self.consume_stream_with_input_estimate_mode(stream, iteration, input_tokens_estimate, true)
            .await
    }

    /// Variant used by internal convergence checkpoints. The response is
    /// assembled normally, but private reflection text is not projected into
    /// the user's streaming transcript.
    pub(super) async fn consume_stream_with_input_estimate_mode(
        &self,
        stream: ChatStream,
        iteration: u32,
        input_tokens_estimate: u32,
        emit_progress: bool,
    ) -> Result<(ChatResponse, bool)> {
        // Thresholds flow from `AgentConfig` (env-overridable: see
        // `OCTOS_LLM_FIRST_TOKEN_GRACE_SECS` / `OCTOS_LLM_STREAM_IDLE_SECS` /
        // `OCTOS_LLM_CALL_MAX_SECS`). The first-token grace caps the
        // input-scaled TTFT budget so a stream that never yields a single
        // token can't hang the turn; the inter-chunk idle catches a stream
        // that stalls mid-flight; `llm_call_max` is the overall wall-clock
        // backstop.
        // Voice fail-fast tightens TTFT / idle (a spoken reply can't wait
        // minutes for the first token) and caps the overall at the voice
        // deadline. Normal turns keep the generous production thresholds.
        let thresholds =
            if octos_llm::current_llm_call_policy() == octos_llm::LlmCallPolicy::FailFast {
                StreamTimeouts {
                    first_token_grace_secs: super::VOICE_STREAM_TTFT_SECS,
                    inter_chunk_idle_secs: super::VOICE_STREAM_IDLE_SECS,
                    overall_max_secs: self.config.voice_overall_deadline.as_secs(),
                }
            } else {
                StreamTimeouts {
                    first_token_grace_secs: self.config.llm_first_token_grace.as_secs(),
                    inter_chunk_idle_secs: self.config.llm_stream_idle.as_secs(),
                    overall_max_secs: self.effective_llm_call_max_secs(),
                }
            };
        self.consume_stream_inner(
            stream,
            iteration,
            input_tokens_estimate,
            thresholds,
            emit_progress,
        )
        .await
    }

    /// Overall wall-clock cap for a normal (non-voice) turn.
    ///
    /// For a local provider (#2229) the cap is disabled by default (`0`) so a
    /// slow-but-healthy reasoning stream is not guillotined mid-flight while
    /// tokens are still flowing — Pi has no such cap, and the inter-chunk idle
    /// and TTFT guards still catch a genuinely dead provider. An operator who
    /// explicitly sets `OCTOS_LLM_CALL_MAX_SECS` gets exactly that value even on
    /// local. Cloud keeps the `DEFAULT_LLM_CALL_MAX_SECS` (1200s) backstop.
    fn effective_llm_call_max_secs(&self) -> u64 {
        if self.is_local_provider() && std::env::var("OCTOS_LLM_CALL_MAX_SECS").is_err() {
            0
        } else {
            self.config.llm_call_max.as_secs()
        }
    }

    async fn consume_stream_inner(
        &self,
        mut stream: ChatStream,
        iteration: u32,
        input_tokens_estimate: u32,
        // Codex round (PR #1355): the inter-chunk idle timeout used to be the
        // only configurable knob. It is now bundled with the first-token
        // grace and an overall wall-clock cap (`StreamTimeouts`) so a stream
        // that never yields, stalls mid-flight, OR trickles a token every
        // <idle>s forever all terminate. Production thresholds come from
        // `AgentConfig` (env-overridable); test fixtures inject tiny values.
        thresholds: StreamTimeouts,
        emit_progress: bool,
    ) -> Result<(ChatResponse, bool)> {
        // Clear any pending status line (e.g., "Thinking...")
        if emit_progress {
            self.reporter().report(ProgressEvent::Response {
                content: String::new(),
                iteration,
            });
        }

        let mut text = String::new();
        let mut reasoning = String::new();
        // MiniMax/Qwen-style models embed chain-of-thought INLINE as
        // <think>…</think> in content deltas (no structured reasoning
        // lane). Route those spans to ReasoningChunk as they arrive so the
        // client's thinking-display toggle governs them — otherwise the raw
        // tags render live in transcripts even though the post-stream strip
        // below cleans the FINAL content (mini4, 2026-07-17).
        let mut think_splitter = octos_llm::ThinkTagStreamSplitter::new();
        // (id, name, args_json, metadata)
        let mut tool_calls: Vec<(String, String, String, Option<serde_json::Value>)> = Vec::new();
        let mut usage = octos_llm::TokenUsage::default();
        let mut stop_reason = StopReason::EndTurn;
        let mut provider_index = None;

        // Adaptive stream timeout:
        // - TTFT (first token): generous — models need time to process large
        //   inputs before generating. Scales with input: base 30s + 1s per 1K
        //   input tokens, capped at the configured first-token grace
        //   (default 180s, env `OCTOS_LLM_FIRST_TOKEN_GRACE_SECS`). The cap
        //   guarantees a stream that never yields a single token still aborts.
        // - Inter-chunk: once streaming starts, the per-poll idle deadline is
        //   the configured stream-idle (default 90s, env
        //   `OCTOS_LLM_STREAM_IDLE_SECS`). A reasoning model legitimately
        //   pausing mid-stream stays under it; a genuinely stalled provider
        //   trips it. Production evidence:
        //   `docs/STREAMING-TRANSACTIONAL-BOUNDARY-ADR.md`.
        // - Overall: a final wall-clock backstop (default 1200s, env
        //   `OCTOS_LLM_CALL_MAX_SECS`) catches a pathological stream that
        //   trickles one token every <idle>s indefinitely — the inter-chunk
        //   guard never trips for it, but the turn must still end.
        let first_token_grace_secs = thresholds.first_token_grace_secs.max(1);
        let inter_chunk_idle_secs = thresholds.inter_chunk_idle_secs.max(1);
        let ttft_secs = (30 + input_tokens_estimate as u64 / 1000).min(first_token_grace_secs);
        let overall_deadline = (thresholds.overall_max_secs > 0).then(|| {
            std::time::Instant::now() + std::time::Duration::from_secs(thresholds.overall_max_secs)
        });
        let mut got_first_chunk = false;
        // Empty deltas affect timeout tracking but do not make EOF a partial answer.
        let mut got_nonempty_output_delta = false;
        // Codex round (PR #1355): track whether we observed an explicit
        // `Done` event. Combined with the tool_calls list this lets us
        // distinguish a real completion from a stream that dropped its
        // terminal signal before delivering all the tool_call args bytes —
        // the latter used to be silently fixed up via "fixing stop_reason"
        // and shipped downstream; now it returns `StreamError::Incomplete`.
        let mut saw_done = false;

        loop {
            // Overall wall-clock backstop: if the configured cap has elapsed,
            // abort now with a retryable idle timeout rather than entering
            // another poll. A stream that trickles one token every <idle>s
            // forever never trips the inter-chunk guard, so this is the only
            // bound that catches it. Computed each iteration so the per-poll
            // idle sleep can also be clamped to never overshoot the cap.
            let overall_remaining = match overall_deadline {
                Some(deadline) => {
                    let now = std::time::Instant::now();
                    if now >= deadline {
                        warn!(
                            "LLM stream stalled: exceeded overall {overall_max}s wall-clock cap \
                             (iteration {iteration})",
                            overall_max = thresholds.overall_max_secs,
                        );
                        return Err(eyre::Report::new(StreamError::IdleTimeout {
                            idle_secs: thresholds.overall_max_secs,
                        }));
                    }
                    Some(deadline - now)
                }
                None => None,
            };

            let idle_timeout = if got_first_chunk {
                std::time::Duration::from_secs(inter_chunk_idle_secs)
            } else {
                std::time::Duration::from_secs(ttft_secs)
            };
            // Never sleep past the overall deadline: clamp the per-poll idle
            // budget so the cap is honored even on the first poll.
            let timeout = match overall_remaining {
                Some(rem) => idle_timeout.min(rem),
                None => idle_timeout,
            };

            let event = tokio::select! {
                biased;
                // Preserve events already buffered by the provider, including
                // usage and completion; preempt instead of awaiting more data.
                event = stream.next() => event,
                _ = self.wait_for_steer_input() => {
                    // This is a sampling boundary, not turn cancellation. Keep
                    // visible text and observed usage, discard unexecuted tool
                    // arguments (possibly incomplete), and let the same turn
                    // drain the new input before its next model request.
                    tool_calls.clear();
                    stop_reason = StopReason::EndTurn;
                    break;
                }
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
                    //
                    // The `timeout` may have been clamped to the overall
                    // wall-clock remainder; the top-of-loop check converts a
                    // clamp-induced wakeup into the overall-cap error on the
                    // next iteration, so the idle-secs reported here is always
                    // the true idle/TTFT budget that elapsed.
                    let idle_secs = if got_first_chunk {
                        inter_chunk_idle_secs
                    } else {
                        ttft_secs
                    };
                    if timeout < idle_timeout {
                        // We slept the clamped remainder, not the full idle
                        // budget — loop so the overall-cap branch fires.
                        continue;
                    }
                    if got_first_chunk {
                        warn!(
                            "LLM stream stalled: no tokens for {idle_secs}s (iteration {iteration})"
                        );
                    } else {
                        warn!(
                            "LLM stream stalled: no first token for {ttft_secs}s \
                             (iteration {iteration}, input_estimate={input_tokens_estimate})"
                        );
                    }
                    return Err(eyre::Report::new(StreamError::IdleTimeout { idle_secs }));
                }
            };

            let Some(event) = event else {
                tracing::debug!("stream ended (None)");
                if !saw_done && got_nonempty_output_delta && tool_calls.is_empty() {
                    return Err(eyre::Report::new(StreamError::Incomplete {
                        detail: "stream ended after output delta(s) before Done event".into(),
                    }));
                }
                break;
            };
            tracing::debug!(?event, "stream event received");

            match event {
                StreamEvent::ProviderIndex(index) => {
                    provider_index = Some(index);
                }
                StreamEvent::ReasoningDelta(delta) => {
                    got_first_chunk = true;
                    got_nonempty_output_delta |= !delta.is_empty();
                    reasoning.push_str(&delta);
                    if emit_progress {
                        self.reporter().report(ProgressEvent::ReasoningChunk {
                            text: delta.clone(),
                            iteration,
                        });
                    }
                }
                StreamEvent::TextDelta(delta) => {
                    got_first_chunk = true;
                    got_nonempty_output_delta |= !delta.is_empty();
                    let (content_part, reasoning_part) = think_splitter.feed(&delta);
                    if !reasoning_part.is_empty() {
                        reasoning.push_str(&reasoning_part);
                        if emit_progress {
                            self.reporter().report(ProgressEvent::ReasoningChunk {
                                text: reasoning_part,
                                iteration,
                            });
                        }
                    }
                    if !content_part.is_empty() {
                        if emit_progress {
                            self.reporter().report(ProgressEvent::StreamChunk {
                                text: content_part.clone(),
                                iteration,
                            });
                        }
                        text.push_str(&content_part);
                    }
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

        // Flush the splitter's holdback (a trailing partial tag was literal
        // text; an unclosed <think> tail is reasoning).
        {
            let (content_tail, reasoning_tail) = think_splitter.finish();
            if !reasoning_tail.is_empty() {
                reasoning.push_str(&reasoning_tail);
            }
            if !content_tail.is_empty() {
                text.push_str(&content_tail);
            }
        }

        let streamed = !text.is_empty() || !reasoning.is_empty();
        if streamed && emit_progress {
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
                    // #1712: distinguish a TRUNCATED call from a genuinely
                    // malformed one. When the turn hit the output token cap
                    // (`finish_reason=length` → StopReason::MaxTokens), the
                    // model was cut off mid-call — the JSON is an unterminated
                    // prefix, not a syntax error. That is retryable
                    // (StreamError::TruncatedToolCall): the retry machinery
                    // re-requests instead of the non-retryable MalformedArgs
                    // killing the turn (which, for a background task with no
                    // "next turn", is instant death). A parse failure after a
                    // CLEAN finish stays MalformedArgs — the #1355 invariant
                    // (let the model see the diagnostic and self-correct) is
                    // preserved.
                    if stop_reason == StopReason::MaxTokens {
                        tracing::warn!(
                            tool = %name,
                            tool_id = %id,
                            error = %e,
                            raw = %truncated_raw,
                            "truncated tool call JSON (output token cap hit mid-call) — surfacing as retryable StreamError::TruncatedToolCall (#1712)"
                        );
                        return Err(eyre::Report::new(StreamError::TruncatedToolCall {
                            tool_id: id,
                            tool_name: name,
                            error: format!("{e} (raw: {truncated_raw})"),
                        }));
                    }
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
        // Replace with a short message so the user sees something useful
        // (#1507: this used to set `None`, which the EndTurn consumer rendered
        // as a completely blank assistant reply).
        let content = if let Some(ref text) = content {
            if Self::is_repetitive_output(text) {
                tracing::warn!(
                    content_len = text.len(),
                    "detected repetitive LLM output, replacing with error message"
                );
                Some(REPETITIVE_OUTPUT_MESSAGE.to_string())
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

    /// Estimate the cost of one response's usage at the model that
    /// actually produced it (resolved via `provider_index`, so failover /
    /// routed slots price at their own model, not the active slot's).
    /// `None` when that model has no catalog pricing. Pass
    /// `provider_index: None` to price at the active slot — used for
    /// tool-reported usage that has no per-response attribution.
    ///
    /// Cache-aware (#1640 follow-up, protocol-aware per #2194 review): the
    /// resolved slot's PROVIDER + MODEL pick the cache rate card
    /// (`pricing::cache_rates` — Anthropic protocol 0.1x/1.25x incl. relabeled
    /// proxies, Gemini 0.25x/0, unknown 1.0x/1.25x so a reported write never
    /// vanishes), and `TokenUsage`'s crate-wide contract is DISJOINT
    /// accounting (inclusive wire formats are normalized at each provider's
    /// parse boundary), so the three counts price independently without
    /// double-billing.
    pub(super) fn response_usage_cost(
        &self,
        input_tokens: u32,
        output_tokens: u32,
        cache_read_tokens: u32,
        cache_write_tokens: u32,
        provider_index: Option<usize>,
    ) -> Option<f64> {
        let metadata = self.llm.provider_metadata_for_index(provider_index);
        octos_llm::pricing::model_pricing(&metadata.model).map(|p| {
            p.cost_with_cache_for_metadata(
                &metadata,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
            )
        })
    }

    /// `attributed_cost` is the caller's per-attempt-priced cost for
    /// exactly `response.usage` (see `call_llm_with_hooks`' return value).
    /// `response.usage` MERGES discarded retry attempts, which can span
    /// provider slots — re-pricing the aggregate at the final slot's rate
    /// (the `None` fallback) misprices cross-provider retries, so callers
    /// that hold the attribution must pass it (codex #1632 r3 P2).
    pub(super) fn emit_cost_update(
        &self,
        turn: &LoopTurnState,
        response: &ChatResponse,
        attributed_cost: Option<f64>,
    ) {
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
        let response_cost = attributed_cost.or_else(|| {
            pricing.map(|p| {
                p.cost_with_cache_for_metadata(
                    &metadata,
                    response_usage.input_tokens,
                    response_usage.output_tokens,
                    response_usage.cache_read_tokens,
                    response_usage.cache_write_tokens,
                )
            })
        });
        // Session figures = completed-runs base + this turn so far.
        //
        // The base comes from the shared session-usage handle (seeded from
        // the usage ledger and folded per completed run by the session
        // actor — each run priced at the model that ran it), so the wire's
        // `session_*` fields survive per-turn resets and the runtime
        // rebuild a `profile/llm/select` switch triggers. The turn part
        // uses `LoopTurnState`'s per-response spend, NOT
        // `pricing.cost(turn totals)` — the old form re-priced the whole
        // turn at the latest answering model whenever failover switched
        // models mid-turn. Without an injected handle (chat mode,
        // sub-agents) the base is zero and the figures stay turn-scoped.
        let total_usage = turn.total_usage();
        let base = self
            .session_usage_base
            .as_ref()
            .map(|handle| handle.snapshot())
            .unwrap_or_default();
        let session_input_tokens = base
            .input_tokens
            .saturating_add(u64::from(total_usage.input_tokens));
        let session_output_tokens = base
            .output_tokens
            .saturating_add(u64::from(total_usage.output_tokens));
        let session_cost = (turn.has_priced_usage() || base.priced_runs > 0)
            .then(|| base.spend_usd + turn.spend_usd());
        // Carry the model id so chat clients can render
        // `model · tokens_in / tokens_out · duration` footers. Skip the
        // synthesis if the provider returns an empty identifier — empty
        // strings would only confuse the client renderer.
        let model = if metadata.model.is_empty() {
            None
        } else {
            Some(metadata.model.clone())
        };
        // Resolve the context window from the SAME slot that answered. For the
        // active slot (`provider_index == None`) use the provider's configured
        // window (honors any context override); for a failover/routed slot
        // derive it from the model that actually answered, parallel to how
        // `model` and `pricing` are resolved above — otherwise the gauge could
        // report the right model with the primary slot's window.
        let context_window = match response.provider_index {
            Some(_) if !metadata.model.is_empty() => {
                octos_llm::context::context_window_tokens(&metadata.model)
            }
            _ => self.llm.context_window(),
        };
        self.reporter().report(ProgressEvent::CostUpdate {
            session_input_tokens,
            session_output_tokens,
            turn_input_tokens: total_usage.input_tokens,
            turn_output_tokens: total_usage.output_tokens,
            response_cost,
            session_cost,
            model,
            context_window: Some(context_window),
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
            source: None,
            participant: None,
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
