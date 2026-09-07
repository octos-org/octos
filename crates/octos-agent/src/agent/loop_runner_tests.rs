use super::*;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};

use async_trait::async_trait;
use octos_core::{AgentId, MessageRole, TaskContext, TaskKind, ToolCall};

// --- compose_turn_user_content (video-call context hint) ---

#[test]
fn should_use_video_call_hint_when_turn_is_flagged_live_video() {
    // `is_video_call` is now the EXPLICIT live-video signal (set from the
    // turn ingress via `inbound.metadata.live_video`), no longer inferred
    // from audio+image attachments.
    let out = compose_turn_user_content("what am I holding", true, true, None);
    assert!(out.starts_with(VIDEO_CALL_NOTE), "got: {out}");
    assert!(out.contains("what am I holding"));
}

#[test]
fn should_use_video_call_hint_even_when_transcript_empty() {
    let out = compose_turn_user_content("", true, true, None);
    assert_eq!(out, VIDEO_CALL_NOTE);
}

#[test]
fn should_keep_uploaded_image_hint_when_not_flagged_video() {
    // Image present but the turn is NOT flagged a live video call → legacy
    // placeholder kept. (A voice note + uploaded image lands here: it must
    // NOT be treated as a camera frame.)
    let out = compose_turn_user_content("", true, false, None);
    assert_eq!(out, "[User sent an image]");
}

#[test]
fn should_not_add_hint_when_no_image() {
    // No image and not flagged → plain transcript passes through.
    let out = compose_turn_user_content("hello there", false, false, None);
    assert_eq!(out, "hello there");
}

#[test]
fn should_not_call_empty_non_image_media_an_image() {
    let out = compose_turn_user_content("", false, false, None);
    assert_eq!(out, "");
}

#[test]
fn should_append_prompt_summary_unchanged_for_non_video_turn() {
    let out = compose_turn_user_content("hi", true, false, Some("SUMMARY"));
    assert_eq!(out, "hi\n\nSUMMARY");
}

#[test]
fn should_combine_video_hint_and_summary() {
    let out = compose_turn_user_content("look", true, true, Some("SUMMARY"));
    assert!(out.starts_with(VIDEO_CALL_NOTE));
    assert!(out.contains("look"));
    assert!(out.ends_with("SUMMARY"));
}
use octos_llm::{
    ChatResponse, LlmError, LlmErrorKind, LlmProvider, StopReason, TokenUsage as LlmTokenUsage,
    ToolChoice,
};
use octos_memory::EpisodeStore;

#[cfg(unix)]
use crate::plugins::PluginTool;
use crate::{AgentConfig, AgentVerifierConfig};

fn tool_use(tool_calls: Vec<ToolCall>, input_tokens: u32, output_tokens: u32) -> ChatResponse {
    ChatResponse {
        content: None,
        reasoning_content: None,
        tool_calls,
        stop_reason: StopReason::ToolUse,
        usage: LlmTokenUsage {
            input_tokens,
            output_tokens,
            ..Default::default()
        },
        provider_index: None,
    }
}

fn end_turn(content: &str, input_tokens: u32, output_tokens: u32) -> ChatResponse {
    ChatResponse {
        content: Some(content.to_string()),
        reasoning_content: None,
        tool_calls: vec![],
        stop_reason: StopReason::EndTurn,
        usage: LlmTokenUsage {
            input_tokens,
            output_tokens,
            ..Default::default()
        },
        provider_index: None,
    }
}

struct ScriptedProvider {
    responses: StdMutex<Vec<ChatResponse>>,
}

impl ScriptedProvider {
    fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            responses: StdMutex::new(responses.into_iter().rev().collect()),
        }
    }
}

#[async_trait]
impl LlmProvider for ScriptedProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &ChatConfig,
    ) -> Result<ChatResponse> {
        self.responses
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pop()
            .ok_or_else(|| eyre::eyre!("scripted provider exhausted"))
    }

    fn model_id(&self) -> &str {
        "planner-test"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

struct StaticResultTool {
    name: &'static str,
    output: &'static str,
    success: bool,
    calls: Arc<AtomicUsize>,
}

/// Unlike the default mock stream adapter, preserve the reasoning channel.
struct TerminalScript(ScriptedProvider);

#[async_trait]
impl LlmProvider for TerminalScript {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[octos_llm::ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatResponse> {
        self.0.chat(messages, tools, config).await
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[octos_llm::ToolSpec],
        config: &ChatConfig,
    ) -> Result<octos_llm::ChatStream> {
        use octos_llm::StreamEvent;
        let response = self.chat(messages, tools, config).await?;
        let events = vec![
            StreamEvent::ReasoningDelta(response.reasoning_content.unwrap_or_default()),
            StreamEvent::TextDelta(response.content.unwrap_or_default()),
            StreamEvent::Usage(response.usage),
            StreamEvent::Done(response.stop_reason),
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
    fn model_id(&self) -> &str {
        "terminal-test"
    }
    fn provider_name(&self) -> &str {
        "mock"
    }
}

#[tokio::test]
async fn terminal_integrity_reasoning_only_recovers_to_actual_answer() {
    let dir = tempfile::tempdir().unwrap();
    let mut reasoning = end_turn("", 10, 20);
    reasoning.reasoning_content = Some("Need to inspect the image.".into());
    let provider = Arc::new(TerminalScript(ScriptedProvider::new(vec![
        reasoning,
        end_turn("Actual final answer", 30, 40),
    ])));
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(
        AgentId::new("terminal-recovery"),
        provider.clone(),
        ToolRegistry::new(),
        memory,
    )
    .with_config(AgentConfig {
        save_episodes: false,
        ..Default::default()
    });
    let result = agent
        .process_message("Inspect the image", &[], vec![])
        .await
        .unwrap();
    assert_eq!(result.content, "Actual final answer");
    assert!(provider.0.responses.lock().unwrap().is_empty());
    assert_eq!(result.token_usage.input_tokens, 40);
    assert_eq!(result.token_usage.output_tokens, 60);
}

#[tokio::test]
async fn terminal_integrity_reasoning_only_fail_fast_is_error() {
    for stop_reason in [StopReason::EndTurn, StopReason::MaxTokens] {
        let dir = tempfile::tempdir().unwrap();
        let mut reasoning = end_turn("", 10, 20);
        reasoning.reasoning_content = Some("Need to inspect the image.".into());
        reasoning.stop_reason = stop_reason;
        let provider = Arc::new(TerminalScript(ScriptedProvider::new(vec![reasoning])));
        let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
        let agent = Agent::new(
            AgentId::new("terminal-no-answer"),
            provider,
            ToolRegistry::new(),
            memory,
        )
        .with_config(AgentConfig {
            save_episodes: false,
            ..Default::default()
        });
        let result = octos_llm::with_llm_call_policy(
            octos_llm::LlmCallPolicy::FailFast,
            agent.process_message("Inspect the image", &[], vec![]),
        )
        .await;
        assert!(
            result.is_err(),
            "reasoning-only output cannot complete: {result:?}"
        );
        let error = result.unwrap_err();
        let usage = &error
            .downcast_ref::<crate::PartialTurnUsage>()
            .unwrap()
            .total;
        assert_eq!((usage.input_tokens, usage.output_tokens), (10, 20));
    }
}

#[tokio::test]
async fn terminal_integrity_exhausted_reasoning_retries_retain_all_usage() {
    let dir = tempfile::tempdir().unwrap();
    let mut reasoning = end_turn("", 10, 20);
    reasoning.reasoning_content = Some("Need to inspect the image.".into());
    let provider = Arc::new(TerminalScript(ScriptedProvider::new(vec![reasoning; 5])));
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(
        AgentId::new("empty-usage"),
        provider,
        ToolRegistry::new(),
        memory,
    )
    .with_config(AgentConfig {
        save_episodes: false,
        ..Default::default()
    });
    let error = agent
        .process_message("Inspect the image", &[], vec![])
        .await
        .unwrap_err();
    let usage = &error
        .downcast_ref::<crate::PartialTurnUsage>()
        .unwrap()
        .total;
    assert_eq!((usage.input_tokens, usage.output_tokens), (50, 100));
}

#[tokio::test]
async fn terminal_integrity_fallback_counts_last_failed_stream_usage() {
    let dir = tempfile::tempdir().unwrap();
    let mut reasoning = end_turn("", 10, 20);
    reasoning.reasoning_content = Some("Need to inspect the image.".into());
    let mut responses = vec![reasoning; 4];
    responses.push(end_turn("Fallback final answer", 30, 40));
    let provider = Arc::new(TerminalScript(ScriptedProvider::new(responses)));
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(
        AgentId::new("fallback-usage"),
        provider,
        ToolRegistry::new(),
        memory,
    )
    .with_config(AgentConfig {
        save_episodes: false,
        ..Default::default()
    });
    let response = agent
        .process_message("Inspect the image", &[], vec![])
        .await
        .unwrap();
    assert_eq!(response.content, "Fallback final answer");
    assert_eq!(
        (
            response.token_usage.input_tokens,
            response.token_usage.output_tokens
        ),
        (70, 120)
    );
}

#[tokio::test]
async fn terminal_integrity_truncated_answer_is_error_with_usage() {
    let dir = tempfile::tempdir().unwrap();
    let mut truncated = end_turn("First I need to inspect the image and then I will", 12, 7);
    truncated.stop_reason = StopReason::MaxTokens;
    let provider = Arc::new(ScriptedProvider::new(vec![truncated]));
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(
        AgentId::new("terminal-truncated"),
        provider,
        ToolRegistry::new(),
        memory,
    )
    .with_config(AgentConfig {
        save_episodes: false,
        ..Default::default()
    });
    let error = agent
        .process_message("Inspect the image", &[], vec![])
        .await
        .expect_err("a truncated response is not a completed answer");
    assert!(error.to_string().contains("max_tokens"), "{error:?}");
    let usage = error.downcast_ref::<crate::PartialTurnUsage>().unwrap();
    assert_eq!(usage.total.input_tokens, 12);
    assert_eq!(usage.total.output_tokens, 7);
}

/// Mix complete-but-rejected responses with transport/provider errors. The
/// response-only fixture above cannot reach the error arm's fallback exits.
struct MixedTerminalScript(StdMutex<VecDeque<Result<ChatResponse>>>);

#[async_trait]
impl LlmProvider for MixedTerminalScript {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &ChatConfig,
    ) -> Result<ChatResponse> {
        self.0
            .lock()
            .unwrap()
            .pop_front()
            .expect("mixed terminal script exhausted")
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[octos_llm::ToolSpec],
        config: &ChatConfig,
    ) -> Result<octos_llm::ChatStream> {
        use octos_llm::StreamEvent;
        let response = self.chat(messages, tools, config).await?;
        Ok(Box::pin(futures::stream::iter(vec![
            StreamEvent::ReasoningDelta(response.reasoning_content.unwrap_or_default()),
            StreamEvent::TextDelta(response.content.unwrap_or_default()),
            StreamEvent::Usage(response.usage),
            StreamEvent::Done(response.stop_reason),
        ])))
    }

    fn model_id(&self) -> &str {
        // Known pricing makes the test check attributed spend as well as all
        // four token counters; no actual provider is contacted.
        "claude-sonnet-4"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

fn mixed_terminal_response(content: &str, input: u32, output: u32) -> ChatResponse {
    let mut response = end_turn(content, input, output);
    response.reasoning_content = Some("Still inspecting the image.".into());
    response.usage.cache_read_tokens = 3;
    response.usage.cache_write_tokens = 4;
    response.usage.reasoning_tokens = 5;
    response
}

fn mixed_terminal_transport_error() -> Result<ChatResponse> {
    Err(octos_llm::StreamError::Transport {
        detail: "fixture transport failure".into(),
    }
    .into())
}

async fn assert_mixed_terminal_usage(
    attempts: Vec<Result<ChatResponse>>,
    expected: (u32, u32, u32, u32),
    succeeds: bool,
) {
    let expected_reasoning: u32 = attempts
        .iter()
        .filter_map(|attempt| attempt.as_ref().ok())
        .map(|response| response.usage.reasoning_tokens)
        .sum();
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(MixedTerminalScript(StdMutex::new(attempts.into())));
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(
        AgentId::new("mixed-terminal-usage"),
        provider.clone(),
        ToolRegistry::new(),
        memory,
    );
    let mut turn = LoopTurnState::new(Instant::now());
    // Existing usage must remain intact, and must not be included again when
    // a recovered response is returned for the caller to record.
    turn.record_usage(
        &TokenUsage {
            input_tokens: 100,
            output_tokens: 200,
            cache_read_tokens: 300,
            cache_write_tokens: 400,
            ..Default::default()
        },
        None,
        Some(0.5),
    );
    let previous = turn.total_usage().clone();
    let result = agent
        .call_llm_with_hooks(
            &[Message::user("Inspect the image")],
            &[],
            &ChatConfig::default(),
            1,
            &previous,
            &mut turn,
        )
        .await;
    assert_eq!(result.is_ok(), succeeds, "{result:?}");
    assert!(provider.0.lock().unwrap().is_empty());
    if let Err(error) = &result {
        assert!(
            error.downcast_ref::<LlmError>().is_some()
                || error.downcast_ref::<octos_llm::StreamError>().is_some(),
            "usage settlement must preserve the typed error: {error:?}",
        );
    }
    if let Ok((response, _, cost)) = result {
        assert_eq!(turn.total_usage().input_tokens, 100);
        assert_eq!(turn.priced_spend(), Some(0.5));
        turn.record_llm_usage(&response.usage, None, cost);
    }
    let total = turn.total_usage();
    assert_eq!(
        total.reasoning_tokens, expected_reasoning,
        "rejected, recovered, and fallback responses retain reasoning exactly once"
    );
    assert_eq!(
        (
            total.input_tokens,
            total.output_tokens,
            total.cache_read_tokens,
            total.cache_write_tokens,
        ),
        (
            100 + expected.0,
            200 + expected.1,
            300 + expected.2,
            400 + expected.3
        ),
    );
    let pricing = octos_llm::pricing::model_pricing("claude-sonnet-4").unwrap();
    // The fixture is a residual-protocol mock, not an Anthropic provider.
    // All disjoint cache traffic must remain priced (read 1x, write 1.25x).
    let expected_cost = pricing.cost(expected.0, expected.1)
        + (f64::from(expected.2) + 1.25 * f64::from(expected.3)) * pricing.input_per_million
            / 1_000_000.0;
    assert!((turn.priced_spend().unwrap() - (0.5 + expected_cost)).abs() < 1e-12);
}

#[tokio::test]
async fn terminal_integrity_mixed_rejected_then_nonretryable_error_retains_usage() {
    assert_mixed_terminal_usage(
        vec![
            Ok(mixed_terminal_response("", 10, 20)),
            Err(LlmError::auth("fixture invalid key").into()),
        ],
        (10, 20, 3, 4),
        false,
    )
    .await;
}

#[tokio::test]
async fn terminal_integrity_mixed_stream_errors_and_empty_fallback_retain_usage() {
    assert_mixed_terminal_usage(
        vec![
            Ok(mixed_terminal_response("", 10, 20)),
            mixed_terminal_transport_error(),
            mixed_terminal_transport_error(),
            mixed_terminal_transport_error(),
            Ok(mixed_terminal_response("", 30, 40)),
        ],
        (40, 60, 6, 8),
        false,
    )
    .await;
}

#[tokio::test]
async fn terminal_integrity_mixed_stream_errors_and_failed_fallback_retain_usage() {
    assert_mixed_terminal_usage(
        vec![
            Ok(mixed_terminal_response("", 10, 20)),
            mixed_terminal_transport_error(),
            mixed_terminal_transport_error(),
            mixed_terminal_transport_error(),
            Err(LlmError::auth("fixture fallback invalid key").into()),
        ],
        (10, 20, 3, 4),
        false,
    )
    .await;
}

#[tokio::test]
async fn terminal_integrity_mixed_stream_recovery_charges_exactly_once() {
    assert_mixed_terminal_usage(
        vec![
            Ok(mixed_terminal_response("", 10, 20)),
            mixed_terminal_transport_error(),
            Ok(mixed_terminal_response("Recovered answer", 30, 40)),
        ],
        (40, 60, 6, 8),
        true,
    )
    .await;
}

#[tokio::test]
async fn terminal_integrity_mixed_stream_fallback_recovery_charges_exactly_once() {
    assert_mixed_terminal_usage(
        vec![
            Ok(mixed_terminal_response("", 10, 20)),
            mixed_terminal_transport_error(),
            mixed_terminal_transport_error(),
            mixed_terminal_transport_error(),
            Ok(mixed_terminal_response("Fallback answer", 30, 40)),
        ],
        (40, 60, 6, 8),
        true,
    )
    .await;
}

#[tokio::test]
async fn terminal_integrity_mixed_adaptive_recovery_keeps_settled_usage_once() {
    let dir = tempfile::tempdir().unwrap();
    // Four rejected streaming attempts and a rejected fallback settle on the
    // first call's Err. The outer agent then retries adaptively and succeeds.
    let mut responses = vec![mixed_terminal_response("", 10, 20); 5];
    responses.push(mixed_terminal_response("Adaptive final answer", 30, 40));
    let provider = Arc::new(TerminalScript(ScriptedProvider::new(responses)));
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(
        AgentId::new("mixed-adaptive-usage"),
        provider.clone(),
        ToolRegistry::new(),
        memory,
    )
    .with_config(AgentConfig {
        save_episodes: false,
        ..Default::default()
    });
    let response = agent
        .process_message("Inspect the image", &[], vec![])
        .await
        .unwrap();
    assert_eq!(response.content, "Adaptive final answer");
    assert_eq!(response.token_usage.reasoning_tokens, 30);
    assert!(provider.0.responses.lock().unwrap().is_empty());
    assert_eq!(
        (
            response.token_usage.input_tokens,
            response.token_usage.output_tokens,
            response.token_usage.cache_read_tokens,
            response.token_usage.cache_write_tokens,
        ),
        (80, 140, 18, 24),
    );
}

impl StaticResultTool {
    fn new(
        name: &'static str,
        output: &'static str,
        success: bool,
        calls: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            name,
            output,
            success,
            calls,
        }
    }
}

#[tokio::test]
async fn should_preserve_tool_carrier_text_in_durable_log_when_final_answer_repeats_it() {
    let dir = tempfile::tempdir().unwrap();
    let answer = "这次我完整读了论文原文。\n\n先纠错。\n\n论文解决了三个问题。";
    let final_content = format!("curl 被拒绝，改用 fetch 工具读取正文：\n\n{answer}");
    let mut tool_response = tool_use(
        vec![ToolCall {
            id: "call_fetch".into(),
            name: "fetch_paper".into(),
            arguments: serde_json::json!({}),
            metadata: None,
        }],
        10,
        20,
    );
    tool_response.content = Some(answer.into());
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_response,
        end_turn(&final_content, 30, 40),
    ]));
    let calls = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(StaticResultTool::new(
        "fetch_paper",
        "paper body",
        true,
        calls.clone(),
    ));
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent =
        Agent::new(AgentId::new("dedupe-test"), provider, tools, memory).with_config(AgentConfig {
            save_episodes: false,
            ..Default::default()
        });

    let response = agent
        .process_message("请读这篇论文", &[], vec![])
        .await
        .expect("turn should complete");

    assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
    assert_eq!(response.content, final_content);
    assert_eq!(response.assistant_segments.message_iterations, vec![(1, 1)]);
    assert_eq!(
        response.assistant_segments.final_iteration, 2,
        "final reply carries its own producer iteration, not the earlier tool carrier"
    );
    assert_eq!(
        response.clone().assistant_segments.message_iterations,
        vec![(1, 1)]
    );
    assert_eq!(response.messages.len(), 3);
    assert_eq!(response.messages[0].role, MessageRole::User);
    assert_eq!(response.messages[1].role, MessageRole::Assistant);
    assert_eq!(response.messages[1].content, answer);
    assert!(
        response.messages[1]
            .tool_calls
            .as_ref()
            .is_some_and(|tool_calls| tool_calls.len() == 1)
    );
    assert_eq!(response.messages[2].role, MessageRole::Tool);
    assert_eq!(
        response.messages[2].tool_call_id.as_deref(),
        Some("call_fetch")
    );
}

#[tokio::test]
async fn should_preserve_tool_carrier_text_in_durable_log_when_turn_ends_on_max_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let answer = "A complete answer emitted before the source check.";
    let final_content = format!("The source check started but output was truncated.\n\n{answer}");
    let mut tool_response = tool_use(
        vec![ToolCall {
            id: "call_fetch".into(),
            name: "fetch_paper".into(),
            arguments: serde_json::json!({}),
            metadata: None,
        }],
        10,
        20,
    );
    tool_response.content = Some(answer.into());
    let max_tokens_response = ChatResponse {
        content: Some(final_content.clone()),
        reasoning_content: None,
        tool_calls: vec![],
        stop_reason: StopReason::MaxTokens,
        usage: LlmTokenUsage::default(),
        provider_index: None,
    };
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_response,
        max_tokens_response,
    ]));
    let mut tools = ToolRegistry::new();
    tools.register(StaticResultTool::new(
        "fetch_paper",
        "paper body",
        true,
        Arc::new(AtomicUsize::new(0)),
    ));
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("max-token-dedupe"), provider, tools, memory).with_config(
        AgentConfig {
            save_episodes: false,
            ..Default::default()
        },
    );

    let error = agent
        .process_message("请读这篇论文", &[], vec![])
        .await
        .expect_err("max-token turn must retain partial output without claiming completion");
    let response = &error
        .downcast_ref::<crate::IncompleteResponseError>()
        .unwrap()
        .partial;

    assert_eq!(response.assistant_segments.message_iterations, vec![(1, 1)]);
    assert_eq!(
        response.assistant_segments.final_iteration, 2,
        "typed partial carries the exact final model iteration through host recovery"
    );
    assert_eq!(
        response.clone().assistant_segments.message_iterations,
        vec![(1, 1)]
    );

    assert_eq!(response.content, final_content);
    assert_eq!(response.messages[1].role, MessageRole::Assistant);
    assert_eq!(response.messages[1].content, answer);
    assert!(
        response.messages[1]
            .tool_calls
            .as_ref()
            .is_some_and(|tool_calls| tool_calls.len() == 1)
    );
    assert_eq!(response.messages[2].role, MessageRole::Tool);
}

/// Scripted provider that records the exact `(messages, tools)` of every
/// request so a test can assert on the prompt SHAPE the loop sent (roles,
/// positions, tool slices), not merely on message text.
struct RequestRecordingProvider {
    responses: StdMutex<Vec<ChatResponse>>,
    requests: RecordedRequests,
}

/// `(messages, tools)` of every provider request, in call order.
type RecordedRequests = Arc<StdMutex<Vec<(Vec<Message>, Vec<octos_llm::ToolSpec>)>>>;

impl RequestRecordingProvider {
    fn new(responses: Vec<ChatResponse>, requests: RecordedRequests) -> Self {
        Self {
            responses: StdMutex::new(responses.into_iter().rev().collect()),
            requests,
        }
    }
}

#[async_trait]
impl LlmProvider for RequestRecordingProvider {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[octos_llm::ToolSpec],
        _config: &ChatConfig,
    ) -> Result<ChatResponse> {
        self.requests
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push((messages.to_vec(), tools.to_vec()));
        self.responses
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pop()
            .ok_or_else(|| eyre::eyre!("scripted provider exhausted"))
    }

    fn model_id(&self) -> &str {
        "planner-test"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

const CHECKPOINT_ENVELOPE_OPEN: &str = "<context_event kind=\"convergence_checkpoint\"";

/// `(messages, tools, config)` of every provider request, in call order.
type RecordedConfigRequests =
    Arc<StdMutex<Vec<(Vec<Message>, Vec<octos_llm::ToolSpec>, ChatConfig)>>>;

/// Like [`RequestRecordingProvider`] but also keeps the `ChatConfig` of each
/// call, so a test can compare the cache-relevant request controls of the
/// checkpoint reflection with those of the action call it shadows.
struct ConfigRecordingProvider {
    responses: StdMutex<Vec<ChatResponse>>,
    requests: RecordedConfigRequests,
}

impl ConfigRecordingProvider {
    fn new(responses: Vec<ChatResponse>, requests: RecordedConfigRequests) -> Self {
        Self {
            responses: StdMutex::new(responses.into_iter().rev().collect()),
            requests,
        }
    }
}

#[async_trait]
impl LlmProvider for ConfigRecordingProvider {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[octos_llm::ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatResponse> {
        self.requests
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push((messages.to_vec(), tools.to_vec(), config.clone()));
        self.responses
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pop()
            .ok_or_else(|| eyre::eyre!("scripted provider exhausted"))
    }

    fn model_id(&self) -> &str {
        "planner-test"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

async fn run_peer_polling_regression(
    tool_name: &'static str,
    outputs: Vec<String>,
    reflection_after: &[usize],
) {
    let dir = tempfile::tempdir().unwrap();
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let mut responses = Vec::new();
    for index in 0..outputs.len() {
        responses.push(tool_use(
            vec![ToolCall {
                id: format!("poll_{index}"),
                name: tool_name.into(),
                arguments: serde_json::json!({}),
                metadata: None,
            }],
            10,
            5,
        ));
        if reflection_after.contains(&(index + 1)) {
            responses.push(end_turn(
                "PRIVATE-POLL-REFLECTION: await new evidence",
                10,
                5,
            ));
        }
    }
    responses.push(end_turn("GENUINE-MODEL-FINAL", 10, 5));
    let expected_calls = responses.len();
    let provider = Arc::new(ConfigRecordingProvider::new(responses, requests.clone()));
    let executions = Arc::new(AtomicUsize::new(0));
    let calls = executions.clone();
    let count = outputs.len();
    let mut tools = ToolRegistry::new();
    if tool_name == "peer_gather" {
        tools.register(crate::tools::PeerGatherTool::new(Arc::new(move |_| {
            let index = calls.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(outputs[index.min(outputs.len() - 1)].clone())
        })));
    } else {
        tools.register(crate::tools::PeerListTool::new(Arc::new(move || {
            let index = calls.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(outputs[index.min(outputs.len() - 1)].clone())
        })));
    }
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("peer-polling"), provider, tools, memory)
        .with_config(AgentConfig {
            save_episodes: false,
            max_iterations: 30,
            ..Default::default()
        })
        .with_convergence_intervals(100, 100_000_000, std::time::Duration::from_secs(86_400));
    let result = agent
        .process_message("gather the peer result", &[], vec![])
        .await
        .unwrap();
    assert_eq!(
        result.content, "GENUINE-MODEL-FINAL",
        "{tool_name}: never substitute a controller stop for a model answer"
    );
    assert_eq!(executions.load(AtomicOrdering::SeqCst), count);
    assert!(!agent.is_loop_detected_recently());
    assert!(
        result
            .messages
            .iter()
            .all(|row| !row.content.contains("PRIVATE-POLL-REFLECTION")),
        "reflection is transient working memory, not a persisted assistant answer"
    );
    assert_eq!(result.token_usage.input_tokens, expected_calls as u32 * 10);
    assert_eq!(result.token_usage.output_tokens, expected_calls as u32 * 5);
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), expected_calls);
    let reflection_requests: Vec<_> = requests
        .iter()
        .filter(|(_, _, config)| matches!(config.tool_choice, ToolChoice::None))
        .collect();
    assert_eq!(reflection_requests.len(), reflection_after.len());
    for (messages, _, _) in reflection_requests {
        let prompt = &messages.last().unwrap().content;
        assert!(
            prompt.contains("peer") && prompt.contains("asynchronous"),
            "waiting-aware checkpoint: {prompt}"
        );
        assert!(
            prompt.contains("busy-wait"),
            "checkpoint must discourage repeated polling: {prompt}"
        );
    }
}

#[tokio::test]
async fn peer_polling_should_allow_changed_result_on_third_identical_request() {
    for tool in ["peer_gather", "peer_list"] {
        run_peer_polling_regression(
            tool,
            vec![
                "still running".into(),
                "still running".into(),
                "done: actual result".into(),
            ],
            &[],
        )
        .await;
    }
}

#[tokio::test]
async fn peer_polling_should_reflect_after_unchanged_results_and_resume_for_genuine_final() {
    for tool in ["peer_gather", "peer_list"] {
        run_peer_polling_regression(
            tool,
            vec![
                "still running".into(),
                "still running".into(),
                "still running".into(),
                "done: actual result".into(),
            ],
            &[3],
        )
        .await;
    }
}

#[tokio::test]
async fn peer_polling_should_reset_no_progress_threshold_when_output_changes() {
    for tool in ["peer_gather", "peer_list"] {
        run_peer_polling_regression(
            tool,
            vec![
                "running: 1".into(),
                "running: 1".into(),
                "running: 2".into(),
                "running: 2".into(),
                "done: result".into(),
            ],
            &[],
        )
        .await;
    }
}

/// The checkpoint request must be the action request plus appended rows:
/// same `context_management`, same reasoning effort (Anthropic derives the
/// `thinking` budget from `max_tokens`, so the output cap must not change it
/// when an effort is configured), and `tool_choice = none` on the wire.
#[tokio::test]
async fn should_send_checkpoint_with_identical_cache_relevant_config_and_tool_choice_none() {
    let dir = tempfile::tempdir().unwrap();
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let provider = Arc::new(ConfigRecordingProvider::new(
        vec![
            tool_use(
                vec![ToolCall {
                    id: "call_fetch_1".into(),
                    name: "fetch_paper".into(),
                    arguments: serde_json::json!({ "page": 1 }),
                    metadata: None,
                }],
                10,
                20,
            ),
            tool_use(
                vec![ToolCall {
                    id: "call_fetch_2".into(),
                    name: "fetch_paper".into(),
                    arguments: serde_json::json!({ "page": 2 }),
                    metadata: None,
                }],
                10,
                20,
            ),
            end_turn("REFLECTION: keep going", 5, 5),
            end_turn("final answer", 10, 10),
        ],
        requests.clone(),
    ));
    let mut tools = ToolRegistry::new();
    tools.register(StaticResultTool::new(
        "fetch_paper",
        "paper body",
        true,
        Arc::new(AtomicUsize::new(0)),
    ));
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("convergence-config"), provider, tools, memory)
        .with_config(AgentConfig {
            save_episodes: false,
            max_tokens: Some(8_192),
            reasoning_effort: Some(octos_llm::ReasoningEffort::High),
            ..Default::default()
        })
        .with_convergence_intervals(2, 100_000_000, std::time::Duration::from_secs(86_400));

    let response = agent
        .process_message("read the paper", &[], vec![])
        .await
        .expect("turn should complete");
    assert_eq!(response.content, "final answer");

    let requests = requests.lock().unwrap_or_else(|error| error.into_inner());
    assert_eq!(requests.len(), 4);
    let (_, _, action) = &requests[1];
    let (_, _, checkpoint) = &requests[2];
    let (_, _, next_action) = &requests[3];
    assert!(matches!(action.tool_choice, octos_llm::ToolChoice::Auto));
    assert!(
        matches!(checkpoint.tool_choice, octos_llm::ToolChoice::None),
        "the reflection must forbid tool use on the wire"
    );
    assert!(matches!(
        next_action.tool_choice,
        octos_llm::ToolChoice::Auto
    ));
    assert_eq!(checkpoint.reasoning_effort, action.reasoning_effort);
    assert_eq!(
        checkpoint.max_tokens, action.max_tokens,
        "with a reasoning effort configured the output cap must not change the thinking budget"
    );
    assert_eq!(checkpoint.context_management, action.context_management);
    assert_eq!(checkpoint.prompt_cache_context, action.prompt_cache_context);
}

/// The single budget-grace call (#1691) belongs to the model's deliverable.
/// When the grace iteration coincides with a due convergence checkpoint, the
/// reflection must not consume it and end the turn without an action call.
#[tokio::test]
async fn should_not_spend_budget_grace_call_on_convergence_reflection() {
    let dir = tempfile::tempdir().unwrap();
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let provider = Arc::new(RequestRecordingProvider::new(
        vec![
            tool_use(
                vec![ToolCall {
                    id: "call_fetch_1".into(),
                    name: "fetch_paper".into(),
                    arguments: serde_json::json!({ "page": 1 }),
                    metadata: None,
                }],
                10,
                20,
            ),
            tool_use(
                vec![ToolCall {
                    id: "call_fetch_2".into(),
                    name: "fetch_paper".into(),
                    arguments: serde_json::json!({ "page": 2 }),
                    metadata: None,
                }],
                10,
                20,
            ),
            end_turn("deliverable", 10, 10),
        ],
        requests.clone(),
    ));
    let mut tools = ToolRegistry::new();
    // Budget grace is granted only after a PRODUCTIVE tool call (a
    // substantive result body, see `is_productive_tool_message`).
    tools.register(StaticResultTool::new(
        "fetch_paper",
        "paper body: the abstract, the method section and the evaluation, long enough to be a substantive tool result rather than a short diagnostic string.",
        true,
        Arc::new(AtomicUsize::new(0)),
    ));
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("convergence-grace"), provider, tools, memory)
        .with_config(AgentConfig {
            save_episodes: false,
            // Two action bodies, then the budget stop is converted into ONE
            // grace call — the same body where a call-interval-2 checkpoint
            // becomes due.
            max_iterations: 2,
            ..Default::default()
        })
        .with_convergence_intervals(2, 100_000_000, std::time::Duration::from_secs(86_400));

    let response = agent
        .process_message("read the paper", &[], vec![])
        .await
        .expect("turn should complete");
    assert_eq!(
        response.content, "deliverable",
        "the grace call must reach the model as an action call"
    );

    let requests = requests.lock().unwrap_or_else(|error| error.into_inner());
    assert_eq!(
        requests.len(),
        3,
        "two action calls plus the grace action call"
    );
    let (grace_messages, _) = &requests[2];
    assert!(
        grace_messages
            .iter()
            .any(|message| message.content.contains("[budget notice]")),
        "the grace request must carry the FINAL-iteration notice"
    );
    assert!(
        grace_messages
            .iter()
            .all(|message| !message.content.contains("CONVERGENCE CHECKPOINT")),
        "a reflection must not spend the grace call"
    );
}

#[tokio::test]
async fn should_send_checkpoint_as_typed_user_tail_with_main_loop_tools_when_convergence_is_due() {
    let dir = tempfile::tempdir().unwrap();
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let reflection_text =
        "REFLECTION: the goal is the paper summary; next action is one bounded fetch.";
    let provider = Arc::new(RequestRecordingProvider::new(
        vec![
            tool_use(
                vec![ToolCall {
                    id: "call_fetch_1".into(),
                    name: "fetch_paper".into(),
                    arguments: serde_json::json!({ "page": 1 }),
                    metadata: None,
                }],
                10,
                20,
            ),
            tool_use(
                vec![ToolCall {
                    id: "call_fetch_2".into(),
                    name: "fetch_paper".into(),
                    arguments: serde_json::json!({ "page": 2 }),
                    metadata: None,
                }],
                10,
                20,
            ),
            end_turn(reflection_text, 5, 5),
            end_turn("final answer", 10, 10),
        ],
        requests.clone(),
    ));
    let mut tools = ToolRegistry::new();
    tools.register(StaticResultTool::new(
        "fetch_paper",
        "paper body",
        true,
        Arc::new(AtomicUsize::new(0)),
    ));
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("convergence-shape"), provider, tools, memory)
        .with_config(AgentConfig {
            save_episodes: false,
            ..Default::default()
        })
        // Only the call axis can fire: a checkpoint is due once two action
        // calls have COMPLETED, i.e. before the third action call.
        .with_convergence_intervals(2, 100_000_000, std::time::Duration::from_secs(86_400));

    let response = agent
        .process_message("read the paper", &[], vec![])
        .await
        .expect("turn should complete");
    assert_eq!(response.content, "final answer");

    let requests = requests.lock().unwrap_or_else(|error| error.into_inner());
    assert_eq!(
        requests.len(),
        4,
        "two action calls, checkpoint reflection call, action call"
    );

    // Stable-prefix contract: no System row may appear after the leading
    // System run in ANY request of the turn (Anthropic hoists every System
    // row into the `system` field, so a tail System row rewrites the prefix).
    for (index, (messages, _)) in requests.iter().enumerate() {
        let leading = messages
            .iter()
            .take_while(|message| message.role == MessageRole::System)
            .count();
        assert!(
            messages[leading..]
                .iter()
                .all(|message| message.role != MessageRole::System),
            "request {index} carries a System row outside the leading run"
        );
    }

    // The checkpoint request is the action request plus appended rows, ends
    // with the checkpoint instruction as a User row, and carries the SAME
    // tool slice as the action call so its serialized prefix can hit the
    // provider cache.
    let (action_messages, action_tools) = &requests[1];
    let (checkpoint_messages, checkpoint_tools) = &requests[2];
    let shape = |messages: &[Message]| {
        messages
            .iter()
            .map(|message| (message.role, message.content.clone()))
            .collect::<Vec<_>>()
    };
    assert!(
        shape(checkpoint_messages).starts_with(&shape(action_messages)),
        "the checkpoint request must extend the action request, not rewrite it"
    );
    let instruction = checkpoint_messages
        .last()
        .expect("checkpoint request has rows");
    assert_eq!(instruction.role, MessageRole::User);
    assert!(instruction.content.contains("CONVERGENCE CHECKPOINT"));
    let tool_names = |tools: &[octos_llm::ToolSpec]| {
        tools
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<Vec<_>>()
    };
    assert!(!action_tools.is_empty());
    assert_eq!(tool_names(checkpoint_tools), tool_names(action_tools));

    // The reflection reaches the next action call as a typed User tail.
    let (next_messages, _) = &requests[3];
    let tail = next_messages.last().expect("next action request has rows");
    assert_eq!(tail.role, MessageRole::User);
    assert!(
        tail.content.starts_with(CHECKPOINT_ENVELOPE_OPEN),
        "reflection must be a typed context_event envelope, got: {}",
        tail.content
    );
    assert!(tail.content.contains(reflection_text));
    assert!(tail.content.trim_end().ends_with("</context_event>"));

    // Transient working memory never enters the durable turn log.
    assert!(response.messages.iter().all(|message| {
        !message.content.contains(CHECKPOINT_ENVELOPE_OPEN)
            && !message.content.contains("CONVERGENCE CHECKPOINT")
    }));
}

fn message_shape(messages: &[Message]) -> Vec<(MessageRole, String)> {
    messages
        .iter()
        .map(|message| (message.role, message.content.clone()))
        .collect()
}

/// Message shape without the transient reflection envelope, which the loop
/// strips and re-appends after the new durable rows on every iteration.
fn durable_shape(messages: &[Message]) -> Vec<(MessageRole, String)> {
    message_shape(messages)
        .into_iter()
        .filter(|(_, content)| !content.starts_with(CHECKPOINT_ENVELOPE_OPEN))
        .collect()
}

fn tool_names(tools: &[octos_llm::ToolSpec]) -> Vec<String> {
    tools.iter().map(|tool| tool.name.clone()).collect()
}

#[tokio::test]
async fn should_fire_call_checkpoints_after_exactly_n_completed_action_calls_when_tools_keep_running()
 {
    let dir = tempfile::tempdir().unwrap();
    let requests: RecordedRequests = Arc::new(StdMutex::new(Vec::new()));
    let action = |page: u64| {
        tool_use(
            vec![ToolCall {
                id: format!("call_{page}"),
                name: "fetch_paper".into(),
                // Distinct arguments per call keep the doom-loop and cycle
                // detectors quiet; only the checkpoint cadence is under test.
                arguments: serde_json::json!({ "page": page }),
                metadata: None,
            }],
            10,
            20,
        )
    };
    let provider = Arc::new(RequestRecordingProvider::new(
        vec![
            action(1),
            action(2),
            action(3),
            end_turn("REFLECTION ONE: keep fetching, one page at a time.", 5, 5),
            action(4),
            action(5),
            action(6),
            end_turn("REFLECTION TWO: three more pages; converging.", 5, 5),
            end_turn("final answer", 10, 10),
        ],
        requests.clone(),
    ));
    let mut tools = ToolRegistry::new();
    tools.register(StaticResultTool::new(
        "fetch_paper",
        "paper body",
        true,
        Arc::new(AtomicUsize::new(0)),
    ));
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("convergence-cadence"), provider, tools, memory)
        .with_config(AgentConfig {
            save_episodes: false,
            ..Default::default()
        })
        .with_convergence_intervals(3, 100_000_000, std::time::Duration::from_secs(86_400));

    let response = agent
        .process_message("read the whole paper", &[], vec![])
        .await
        .expect("turn should complete");
    assert_eq!(response.content, "final answer");

    let requests = requests.lock().unwrap_or_else(|error| error.into_inner());
    let is_checkpoint = |messages: &[Message]| {
        messages.last().is_some_and(|message| {
            message.role == MessageRole::User && message.content.contains("CONVERGENCE CHECKPOINT")
        })
    };
    let checkpoint_indices = requests
        .iter()
        .enumerate()
        .filter(|(_, (messages, _))| is_checkpoint(messages))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    // 3 actions, checkpoint, 3 actions, checkpoint, final action: nine
    // requests, with the checkpoints as the 4th and 8th (0-based 3 and 7).
    assert_eq!(
        requests.len(),
        9,
        "expected 7 action requests and 2 checkpoint requests"
    );
    assert_eq!(
        checkpoint_indices,
        vec![3, 7],
        "a call-based checkpoint fires after exactly 3 COMPLETED action calls, \
         and the reflection call must not count toward the next one"
    );
    for index in checkpoint_indices {
        let (messages, tools) = &requests[index];
        let instruction = messages.last().expect("checkpoint request has rows");
        assert!(
            instruction.content.contains("3 LLM action calls"),
            "checkpoint {index} must report the completed action calls, got: {}",
            instruction.content
        );
        assert_eq!(tool_names(tools), tool_names(&requests[0].1));
        // The transient reflection envelope is stripped and re-appended after
        // the new durable rows each iteration, so compare durable rows only.
        assert!(
            durable_shape(messages).starts_with(&durable_shape(&requests[index - 1].0)),
            "checkpoint {index} must extend the preceding action request's durable rows"
        );
    }
    // Each reflection reaches the following action call as the typed tail.
    let tail = |index: usize| {
        requests[index]
            .0
            .last()
            .expect("request has rows")
            .content
            .clone()
    };
    assert!(tail(4).starts_with(CHECKPOINT_ENVELOPE_OPEN) && tail(4).contains("REFLECTION ONE"));
    assert!(tail(8).starts_with(CHECKPOINT_ENVELOPE_OPEN) && tail(8).contains("REFLECTION TWO"));
}

/// Simulates an `AdaptiveRouter` whose per-call slot selection flaps:
/// `provider_name()`/`model_id()` alternate on every request. Records the
/// `(affinity_key, epoch_id)` each request carried on its `ChatConfig`.
struct FlappingRouteProvider {
    calls: AtomicUsize,
    responses: StdMutex<Vec<ChatResponse>>,
    observed_cache_identity: Arc<StdMutex<Vec<(String, String)>>>,
}

#[async_trait]
impl LlmProvider for FlappingRouteProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatResponse> {
        let context = config
            .prompt_cache_context
            .as_ref()
            .expect("agent attaches a prompt cache context to every call");
        self.observed_cache_identity
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push((context.affinity_key.clone(), context.epoch_id.clone()));
        self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        self.responses
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pop()
            .ok_or_else(|| eyre::eyre!("scripted provider exhausted"))
    }

    fn model_id(&self) -> &str {
        if self.calls.load(AtomicOrdering::SeqCst) % 2 == 0 {
            "gpt-5"
        } else {
            "claude-fallback"
        }
    }

    fn provider_name(&self) -> &str {
        if self.calls.load(AtomicOrdering::SeqCst) % 2 == 0 {
            "openai"
        } else {
            "anthropic"
        }
    }
}

#[tokio::test]
async fn should_keep_prompt_cache_affinity_stable_when_router_selection_flaps_mid_turn() {
    let dir = tempfile::tempdir().unwrap();
    let observed = Arc::new(StdMutex::new(Vec::new()));
    let provider = Arc::new(FlappingRouteProvider {
        calls: AtomicUsize::new(0),
        // Popped from the back: tool round first, then the final answer.
        responses: StdMutex::new(vec![
            end_turn("final answer", 3, 3),
            tool_use(
                vec![ToolCall {
                    id: "call_fetch".into(),
                    name: "fetch_paper".into(),
                    arguments: serde_json::json!({}),
                    metadata: None,
                }],
                10,
                20,
            ),
        ]),
        observed_cache_identity: observed.clone(),
    });
    let mut tools = ToolRegistry::new();
    tools.register(StaticResultTool::new(
        "fetch_paper",
        "paper body",
        true,
        Arc::new(AtomicUsize::new(0)),
    ));
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("route-flap"), provider, tools, memory)
        .with_config(AgentConfig {
            save_episodes: false,
            ..Default::default()
        })
        .with_parent_session_key("api:private-route-flap-session");

    agent
        .process_message("read the paper", &[], vec![])
        .await
        .expect("turn should complete");

    let observed = observed.lock().unwrap_or_else(|error| error.into_inner());
    assert_eq!(observed.len(), 2, "one call per routed slot");
    let (first_affinity, first_epoch) = &observed[0];
    let (second_affinity, second_epoch) = &observed[1];
    assert_eq!(
        first_affinity, second_affinity,
        "prompt_cache_key must not follow the router's per-call slot selection"
    );
    assert_eq!(
        first_epoch, second_epoch,
        "the non-OUP fallback epoch must not rotate on a route flap"
    );
    assert!(first_affinity.len() <= 64);
    assert!(!first_affinity.contains("private-route-flap-session"));
}

#[async_trait]
impl Tool for StaticResultTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "test tool for verifier loop tests"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }

    async fn execute(&self, _args: &serde_json::Value) -> Result<ToolResult> {
        self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        Ok(ToolResult {
            output: self.output.to_string(),
            success: self.success,
            ..Default::default()
        })
    }
}

/// Planner that keeps requesting a tool call (so the loop runs to its
/// iteration cap) and records whether it ever received the in-band budget
/// reminder (#1691).
struct BudgetProbePlanner {
    calls: Arc<AtomicUsize>,
    saw_notice: Arc<AtomicBool>,
}

#[async_trait]
impl LlmProvider for BudgetProbePlanner {
    async fn chat(
        &self,
        messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &ChatConfig,
    ) -> Result<ChatResponse> {
        if messages
            .iter()
            .any(|m| m.content.contains("[budget notice]"))
        {
            self.saw_notice.store(true, AtomicOrdering::SeqCst);
        }
        let n = self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        // Never end the turn — force the loop toward its iteration cap.
        Ok(tool_use(
            vec![ToolCall {
                id: format!("call_{n}"),
                name: "noop_tool".to_string(),
                arguments: serde_json::json!({ "n": n }),
                metadata: None,
            }],
            10,
            5,
        ))
    }

    fn model_id(&self) -> &str {
        "budget-probe"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

#[tokio::test]
async fn budget_reminder_injected_before_iteration_cap() {
    // #1691: as a run approaches its iteration cap the model must receive an
    // in-band "wrap up and deliver" reminder, so it converges on a
    // deliverable instead of silently hitting the wall (the mini4 review
    // worker burned all 50 iterations and wrote nothing).
    let dir = tempfile::tempdir().unwrap();
    let saw_notice = Arc::new(AtomicBool::new(false));
    let planner = Arc::new(BudgetProbePlanner {
        calls: Arc::new(AtomicUsize::new(0)),
        saw_notice: saw_notice.clone(),
    });
    let mut tools = ToolRegistry::new();
    tools.register(StaticResultTool::new(
        "noop_tool",
        "ok",
        true,
        Arc::new(AtomicUsize::new(0)),
    ));
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent =
        Agent::new(AgentId::new("budget-probe"), planner, tools, memory).with_config(AgentConfig {
            max_iterations: 5,
            save_episodes: false,
            ..Default::default()
        });

    let _ = agent.process_message("do a long task", &[], vec![]).await;

    assert!(
        saw_notice.load(AtomicOrdering::SeqCst),
        "the model never received the pre-cap budget reminder (#1691)"
    );
}

struct NoCallVerifier {
    calls: AtomicUsize,
}

#[async_trait]
impl LlmProvider for NoCallVerifier {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &ChatConfig,
    ) -> Result<ChatResponse> {
        self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        eyre::bail!("verifier should have been skipped for quiet progress")
    }

    fn model_id(&self) -> &str {
        "haiku-test"
    }

    fn provider_name(&self) -> &str {
        "mock-verifier"
    }
}

struct RepeatAwarePlanner {
    calls: AtomicUsize,
    saw_repeating_note: AtomicBool,
}

impl RepeatAwarePlanner {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            saw_repeating_note: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl LlmProvider for RepeatAwarePlanner {
    async fn chat(
        &self,
        messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &ChatConfig,
    ) -> Result<ChatResponse> {
        let call = self.calls.fetch_add(1, AtomicOrdering::SeqCst) + 1;
        let saw_repeating = messages.iter().any(|message| {
            message.content.contains("[verifier]") && message.content.contains("verdict: Repeating")
        });
        if saw_repeating {
            self.saw_repeating_note.store(true, AtomicOrdering::SeqCst);
        }
        let fix_ran = messages.iter().any(|message| {
            message.role == MessageRole::Tool && message.content.contains("fixed style")
        });
        if fix_ran {
            return Ok(end_turn("fixed answer", 12, 6));
        }
        if saw_repeating {
            return Ok(tool_use(
                vec![ToolCall {
                    id: "fix_call".into(),
                    name: "fix_tool".into(),
                    arguments: serde_json::json!({"path": "style.toml", "repair": true}),
                    metadata: None,
                }],
                10,
                5,
            ));
        }
        Ok(tool_use(
            vec![ToolCall {
                id: format!("fail_call_{call}"),
                name: "fail_tool".into(),
                arguments: serde_json::json!({"path": "style.toml"}),
                metadata: None,
            }],
            10,
            5,
        ))
    }

    fn model_id(&self) -> &str {
        "planner-test"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

struct LedgerDrivenVerifier {
    calls: AtomicUsize,
}

#[async_trait]
impl LlmProvider for LedgerDrivenVerifier {
    async fn chat(
        &self,
        messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatResponse> {
        self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        assert!(
            matches!(config.tool_choice, ToolChoice::None),
            "verifier call must not expose tools"
        );
        assert_eq!(
            octos_llm::current_lane_context().lane,
            Some(octos_llm::Lane::FastChat),
            "verifier call should use the cheap fast-chat lane"
        );
        let prompt = messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let verdict = if prompt.contains("tool=fix_tool") {
            r#"{"verdict":"ReadyToAnswer"}"#
        } else if prompt.contains("repeating=true") {
            r#"{"verdict":"Repeating","error_class":"ContractFail"}"#
        } else {
            r#"{"verdict":"Insufficient","reason":"need a different action"}"#
        };
        Ok(ChatResponse {
            content: Some(verdict.to_string()),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: LlmTokenUsage {
                input_tokens: 3,
                output_tokens: 2,
                ..Default::default()
            },
            provider_index: None,
        })
    }

    fn model_id(&self) -> &str {
        "haiku-test"
    }

    fn provider_name(&self) -> &str {
        "mock-verifier"
    }
}

struct PrematureEndPlanner {
    calls: AtomicUsize,
}

#[async_trait]
impl LlmProvider for PrematureEndPlanner {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &ChatConfig,
    ) -> Result<ChatResponse> {
        let call = self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        if call == 0 {
            return Ok(tool_use(
                vec![ToolCall {
                    id: "fail_once".into(),
                    name: "fail_tool".into(),
                    arguments: serde_json::json!({"path": "style.toml"}),
                    metadata: None,
                }],
                8,
                4,
            ));
        }
        if call == 1 {
            return Ok(end_turn("premature answer", 8, 4));
        }
        Ok(end_turn("ready answer", 8, 4))
    }

    fn model_id(&self) -> &str {
        "planner-test"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

struct GateVerifier {
    calls: AtomicUsize,
}

#[async_trait]
impl LlmProvider for GateVerifier {
    async fn chat(
        &self,
        messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &ChatConfig,
    ) -> Result<ChatResponse> {
        let call = self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        let prompt = messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let verdict = if prompt.contains("Proposed answer:\nready answer") {
            r#"{"verdict":"ReadyToAnswer"}"#
        } else if call == 0 {
            r#"{"verdict":"Blocked","reason":"tool failed"}"#
        } else {
            r#"{"verdict":"Insufficient","reason":"not ready yet"}"#
        };
        Ok(ChatResponse {
            content: Some(verdict.to_string()),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: LlmTokenUsage {
                input_tokens: 3,
                output_tokens: 2,
                ..Default::default()
            },
            provider_index: None,
        })
    }

    fn model_id(&self) -> &str {
        "haiku-test"
    }

    fn provider_name(&self) -> &str {
        "mock-verifier"
    }
}
#[cfg(unix)]
use crate::plugins::manifest::PluginToolDef;
use crate::prompt_context::{
    PromptContextManager, PromptContextPhase, PromptContextReport, PromptContextRequest,
};
#[cfg(unix)]
use crate::tools::TurnAttachmentContext;
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

struct TerminalFailureTool {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for TerminalFailureTool {
    fn name(&self) -> &str {
        "lesson_generate"
    }

    fn description(&self) -> &str {
        "Generate one complete lesson"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "tutor_context": { "type": "string" } }
        })
    }

    async fn execute(&self, _args: &serde_json::Value) -> Result<ToolResult> {
        self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        Ok(ToolResult {
            output: "lesson generation exhausted its internal attempts".to_string(),
            success: false,
            structured_metadata: Some(serde_json::json!({
                "retryable": false,
                "do_not_retry_same_turn": true
            })),
            ..Default::default()
        })
    }
}

struct TerminalLessonRetryProvider {
    calls: AtomicUsize,
}

#[async_trait]
impl LlmProvider for TerminalLessonRetryProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> Result<ChatResponse> {
        let call = self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        let suffix = if call == 0 { "first" } else { "rewritten" };
        Ok(ChatResponse {
            content: None,
            reasoning_content: None,
            tool_calls: vec![ToolCall {
                id: format!("call_lesson_{call}"),
                name: "lesson_generate".to_string(),
                // The actual incident changed context strings on every retry.
                // The guard must key on terminal tool identity, not exact args.
                arguments: serde_json::json!({ "tutor_context": suffix }),
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

#[cfg(unix)]
struct ConsecutiveVoiceSaveProvider {
    calls: AtomicUsize,
}

#[cfg(unix)]
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
async fn terminal_tool_failure_blocks_a_rewritten_retry_in_the_same_turn() {
    let dir = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::with_builtins(dir.path());
    tools.register(TerminalFailureTool {
        calls: Arc::clone(&calls),
    });

    let provider: Arc<dyn LlmProvider> = Arc::new(TerminalLessonRetryProvider {
        calls: AtomicUsize::new(0),
    });
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("test-agent"), provider, tools, memory);

    let result = agent
        .process_message("teach me", &[], vec![])
        .await
        .unwrap();

    assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
    assert_eq!(
        result.content,
        terminal_tool_retry_message("lesson_generate"),
    );
    assert_eq!(
        result
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::Tool)
            .count(),
        1,
        "the rewritten second call must be stopped before it creates another tool result",
    );
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
        contexts: vec![],
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
                live_video: false,
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
                live_video: false,
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
fn recover_shell_retry_does_not_fire_diff_like_on_all_success_turn() {
    // P2 (tri-repo #1529): DiffLikeSuccess used to fire whenever ANY recent
    // shell result was diff-like, regardless of failures. A legitimate turn
    // that runs `git diff` >= threshold times (all exit 0) is NOT a spiral
    // and must NOT be short-circuited into surfacing the raw diff as the
    // assistant answer. Build a 4-shell all-success diff streak and assert
    // recovery does not fire.
    let diff =
        "diff --git a/x.rs b/x.rs\n--- a/x.rs\n+++ b/x.rs\n@@ -1 +1 @@\n-a\n+b\n\nExit code: 0";
    let mut messages = vec![Message::user("show me every diff")];
    for i in 0..4 {
        let id = format!("call_diff_{i}");
        messages.push(Message {
            role: MessageRole::Assistant,
            content: String::new(),
            media: vec![],
            tool_calls: Some(vec![ToolCall {
                id: id.clone(),
                name: "shell".into(),
                arguments: serde_json::json!({"command": "git diff"}),
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
            content: diff.into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: Some(id),
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        });
    }

    assert!(
        recover_shell_retry(&messages, 4).is_none(),
        "an all-success diff streak must not trigger shell-spiral recovery"
    );
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
    let shell_idx = latest_tool_batch_index(&messages, "shell").expect("shell present in batch");
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

#[tokio::test]
async fn verifier_skips_quiet_successful_tool_batch_and_persists_ledger() {
    let dir = tempfile::tempdir().unwrap();
    let ledger_path = dir.path().join("turn_ledger.jsonl");
    let planner: Arc<dyn LlmProvider> = Arc::new(ScriptedProvider::new(vec![
        tool_use(
            vec![ToolCall {
                id: "quiet_call".into(),
                name: "quiet_tool".into(),
                arguments: serde_json::json!({"path": "ok.txt"}),
                metadata: None,
            }],
            10,
            5,
        ),
        end_turn("done", 10, 5),
    ]));
    let verifier = Arc::new(NoCallVerifier {
        calls: AtomicUsize::new(0),
    });
    let mut tools = ToolRegistry::new();
    tools.register(StaticResultTool::new(
        "quiet_tool",
        "healthy progress\n\nExit code: 0",
        true,
        Arc::new(AtomicUsize::new(0)),
    ));
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("verifier-skip"), planner, tools, memory)
        .with_config(AgentConfig {
            save_episodes: false,
            ..Default::default()
        })
        .with_verifier_config(
            AgentVerifierConfig::with_provider(verifier.clone(), "haiku-test")
                .with_ledger_path(&ledger_path),
        );

    let response = agent
        .process_message("do one safe thing", &[], vec![])
        .await
        .unwrap();

    assert_eq!(response.content, "done");
    assert_eq!(verifier.calls.load(AtomicOrdering::SeqCst), 0);
    let persisted = std::fs::read_to_string(&ledger_path).expect("turn ledger persisted");
    assert!(
        persisted.contains("\"tool\":\"quiet_tool\""),
        "quiet successful tool call should still be ledgered: {persisted}"
    );
}

#[tokio::test]
async fn verifier_repeating_note_changes_next_planner_action() {
    let dir = tempfile::tempdir().unwrap();
    let ledger_path = dir.path().join("turn_ledger.jsonl");
    let planner = Arc::new(RepeatAwarePlanner::new());
    let verifier = Arc::new(LedgerDrivenVerifier {
        calls: AtomicUsize::new(0),
    });
    let fail_calls = Arc::new(AtomicUsize::new(0));
    let fix_calls = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(StaticResultTool::new(
        "fail_tool",
        "[VALIDATION FAILED] style TOML is malformed",
        false,
        fail_calls.clone(),
    ));
    tools.register(StaticResultTool::new(
        "fix_tool",
        "fixed style\n\nExit code: 0",
        true,
        fix_calls.clone(),
    ));
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(
        AgentId::new("verifier-repeat"),
        planner.clone(),
        tools,
        memory,
    )
    .with_config(AgentConfig {
        max_iterations: 12,
        save_episodes: false,
        ..Default::default()
    })
    .with_verifier_config(
        AgentVerifierConfig::with_provider(verifier.clone(), "haiku-test")
            .with_ledger_path(&ledger_path),
    );

    let response = agent
        .process_message("repair the generated style", &[], vec![])
        .await
        .unwrap();

    assert_eq!(response.content, "fixed answer");
    assert!(
        planner.saw_repeating_note.load(AtomicOrdering::SeqCst),
        "planner must observe the injected Repeating verifier note"
    );
    assert_eq!(fail_calls.load(AtomicOrdering::SeqCst), 3);
    assert_eq!(fix_calls.load(AtomicOrdering::SeqCst), 1);
    assert!(
        verifier.calls.load(AtomicOrdering::SeqCst) >= 4,
        "verifier should classify failures and the ready gate"
    );
    let persisted = std::fs::read_to_string(&ledger_path).expect("turn ledger persisted");
    assert!(persisted.contains("\"tool\":\"fail_tool\""));
    assert!(persisted.contains("\"tool\":\"fix_tool\""));
}

#[tokio::test]
async fn verifier_ready_to_answer_gates_endturn_after_problem_signal() {
    let dir = tempfile::tempdir().unwrap();
    let planner = Arc::new(PrematureEndPlanner {
        calls: AtomicUsize::new(0),
    });
    let verifier = Arc::new(GateVerifier {
        calls: AtomicUsize::new(0),
    });
    let mut tools = ToolRegistry::new();
    tools.register(StaticResultTool::new(
        "fail_tool",
        "[VALIDATION FAILED] artifact missing",
        false,
        Arc::new(AtomicUsize::new(0)),
    ));
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(
        AgentId::new("verifier-gate"),
        planner.clone(),
        tools,
        memory,
    )
    .with_config(AgentConfig {
        max_iterations: 8,
        save_episodes: false,
        ..Default::default()
    })
    .with_verifier_config(AgentVerifierConfig::with_provider(
        verifier.clone(),
        "haiku-test",
    ));

    let response = agent
        .process_message("try then answer too early", &[], vec![])
        .await
        .unwrap();

    assert_eq!(response.content, "ready answer");
    assert!(
        planner.calls.load(AtomicOrdering::SeqCst) >= 3,
        "premature EndTurn must be rejected until ReadyToAnswer"
    );
    assert!(
        verifier.calls.load(AtomicOrdering::SeqCst) >= 3,
        "failure classification plus two termination checks expected"
    );
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
async fn shell_spiral_dispatch_marks_loop_detected_recently() {
    // #1656: a firing shell spiral must mark the two-stage dedup flag, so a
    // generic loop detection later in the SAME turn is treated as the second
    // fire (terminal) instead of restarting the warn-then-terminate ladder.
    let dir = tempfile::tempdir().unwrap();
    let agent = build_agent_with_mock(dir.path()).await;

    // Four consecutive failing shell exchanges inside the current user turn —
    // the spiral detector's threshold.
    let mut messages = vec![Message::user("fix the build")];
    for i in 0..4 {
        let call_id = format!("call_shell_{i}");
        messages.push(Message {
            role: MessageRole::Assistant,
            content: String::new(),
            media: vec![],
            tool_calls: Some(vec![ToolCall {
                id: call_id.clone(),
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
            content: "error[E0999]: broken\n\nExit code: 101".into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: Some(call_id),
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        });
    }

    assert!(!agent.is_loop_detected_recently());
    let mut retry_state = LoopRetryState::new();
    let outcome = agent.dispatch_shell_retry_recovery(&messages, &mut retry_state, 1);
    assert!(
        outcome.is_some(),
        "the spiral must fire on 4 failing shells"
    );
    assert!(
        agent.is_loop_detected_recently(),
        "a firing spiral must mark the loop-detected flag (#1656)"
    );

    // The NEXT generic loop detection in this turn is the SECOND fire —
    // terminal, not a fresh warning.
    let second = agent.dedup_loop_warning("[LOOP DETECTED] generic".to_string());
    assert!(
        second.is_err(),
        "generic detection after a spiral must be terminal, got {second:?}"
    );
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
    let runner =
        CompactionRunner::new(policy).with_summarizer(CountingSummarizer { calls: spy.clone() });
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
    let mut retry_state = LoopRetryState::with_limits(crate::agent::loop_state::LoopRetryLimits {
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
    let memory = runtime
        .block_on(async { Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap()) });
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
    let memory = runtime
        .block_on(async { Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap()) });
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
async fn doom_loop_aborts_turn_when_third_identical_call_arrives() {
    // #1765 doom-loop guard: when the LLM issues the SAME tool call
    // (same name + identical arguments JSON) 3 times in a row, the
    // conversation loop must abort the turn with a clear message
    // instead of issuing the next LLM call.
    //
    // We assert via:
    //   - The terminal `content` matches `doom_loop_terminal_message`
    //     (proves the doom guard fired, not the older two-stage
    //     warn-then-terminate cycle path).
    //   - The mock LLM was called EXACTLY 3 times: calls 1 and 2
    //     execute the tool; the 3rd identical call trips the guard and
    //     no further LLM call is issued.
    //   - The flag (`is_loop_detected_recently`) is set after the run.
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
        .expect("process_message should return Ok even when the doom guard aborts");

    assert_eq!(
        result.content,
        doom_loop_terminal_message("read_file", 3),
        "expected the doom-loop abort message when the 3rd identical \
             call arrives"
    );
    assert!(agent.is_loop_detected_recently());

    let total_calls = provider.calls.load(AtomicOrdering::SeqCst);
    assert_eq!(
        total_calls, 3,
        "expected exactly 3 LLM calls — the doom guard must stop the \
             loop instead of issuing a 4th; got {total_calls}"
    );
}

/// #1969 mock: call 1 returns a tool_use that burns real tokens
/// (input=1000, output=500); call 2 rate-limits. Under FailFast the loop
/// bails on call 2 — and the accumulated usage from call 1 must ride out on
/// the error instead of being dropped by the bare `return Err(report)`.
struct UsageThenRateLimitProvider {
    calls: AtomicUsize,
}

#[async_trait]
impl LlmProvider for UsageThenRateLimitProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> Result<ChatResponse> {
        let n = self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        if n == 0 {
            Ok(tool_use(
                vec![ToolCall {
                    id: "call_0".to_string(),
                    name: "noop_tool".to_string(),
                    arguments: serde_json::json!({}),
                    metadata: None,
                }],
                1000,
                500,
            ))
        } else {
            // Typed provider rate-limit; under FailFast the loop bails here.
            Err(LlmError::rate_limited(Some(2)).into())
        }
    }

    fn model_id(&self) -> &str {
        "mock"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

#[tokio::test]
async fn should_surface_accumulated_usage_when_llm_errors_after_prior_iteration() {
    // #1969: the turn accumulates usage in `LoopTurnState.total_usage` and the
    // happy path returns it via `ConversationResponse.token_usage`. Every error
    // exit was a bare `return Err(report)`, dropping that usage — so an
    // errored/rate-limited peer or goal turn charged 0 tokens despite having
    // burned real tokens on earlier iterations. The bailed error must now carry
    // the accumulated usage, WITHOUT hiding the underlying `LlmError` that
    // retry/breaker classification depends on.
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(UsageThenRateLimitProvider {
        calls: AtomicUsize::new(0),
    });
    let provider_arc: Arc<dyn LlmProvider> = provider.clone();
    let mut tools = ToolRegistry::new();
    tools.register(StaticResultTool::new(
        "noop_tool",
        "ok",
        true,
        Arc::new(AtomicUsize::new(0)),
    ));
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("usage-error"), provider_arc, tools, memory);

    // FailFast makes call 2's rate-limit terminal (no retry/failover), so the
    // loop bails deterministically after recording call 1's usage.
    let err = octos_llm::with_llm_call_policy(
        octos_llm::LlmCallPolicy::FailFast,
        agent.process_message("please work", &[], vec![]),
    )
    .await
    .expect_err("rate-limit under FailFast must bail the turn");

    // Constraint: the underlying LlmError must remain downcastable so
    // `classify_report` / retry-breaker logic still sees the rate limit.
    assert!(
        err.downcast_ref::<octos_llm::LlmError>().is_some(),
        "underlying LlmError must stay downcastable through the usage carrier"
    );

    // The bailed error carries call 1's accumulated usage.
    let partial = err
        .downcast_ref::<crate::PartialTurnUsage>()
        .expect("bailed error must carry the turn's accumulated usage");
    assert_eq!(
        partial.total.input_tokens, 1000,
        "accumulated input tokens from iteration 1 must survive the error exit"
    );
    assert_eq!(
        partial.total.output_tokens, 500,
        "accumulated output tokens from iteration 1 must survive the error exit"
    );
}

/// LLM mock that alternates between two different argument sets for the
/// same tool. The doom guard (consecutive identical) must never fire;
/// the cycle detector (`LoopDetector::record`) owns alternating
/// patterns and still runs its two-stage warn-then-terminate recovery.
struct CountingAlternatingArgsProvider {
    calls: AtomicUsize,
}

#[async_trait]
impl LlmProvider for CountingAlternatingArgsProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> Result<ChatResponse> {
        let n = self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        let path = if n % 2 == 0 { "a.txt" } else { "b.txt" };
        Ok(ChatResponse {
            content: None,
            reasoning_content: None,
            tool_calls: vec![ToolCall {
                id: format!("call_alt_{n}"),
                name: "read_file".to_string(),
                arguments: serde_json::json!({ "path": path }),
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
async fn alternating_cycle_still_uses_two_stage_warning_not_doom_abort() {
    // #1765: the doom guard counts CONSECUTIVE identical calls only —
    // an A,B,A,B,… alternation resets the streak every call, so the
    // existing cycle detector must keep owning that pattern with its
    // two-stage recovery (first fire injects a warning + one more LLM
    // iteration; second fire terminates with
    // `loop_detected_terminal_message`).
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
    std::fs::write(dir.path().join("b.txt"), b"b").unwrap();
    let provider = Arc::new(CountingAlternatingArgsProvider {
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
        .process_message("please alternate", &[], vec![])
        .await
        .expect("process_message should return Ok when the cycle detector terminates");

    assert_eq!(
        result.content,
        loop_detected_terminal_message(),
        "alternating A,B cycles belong to the two-stage cycle detector, \
             not the doom guard"
    );
    let total_calls = provider.calls.load(AtomicOrdering::SeqCst);
    assert!(
        total_calls >= 7,
        "expected >= 7 LLM calls (6 to reach a cycle-2 first fire + 1 \
             recovery iteration before the terminating fire); got {total_calls}"
    );
}

/// LLM mock that always calls a named tool, so the loop detector fires
/// repeatedly on a tool whose EXECUTION count the test can observe.
struct CountingAlwaysNamedToolProvider {
    calls: AtomicUsize,
    tool_name: &'static str,
}

#[async_trait]
impl LlmProvider for CountingAlwaysNamedToolProvider {
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
                name: self.tool_name.to_string(),
                arguments: serde_json::json!({}),
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
async fn doom_loop_does_not_execute_the_tripping_call() {
    // #1765: the doom guard runs BEFORE tool execution. When the 3rd
    // identical call arrives it must abort the turn without executing
    // the call again — re-running an identical call is pure waste (and
    // possibly a duplicated side effect).
    //
    // With the same-call provider:
    //   - iterations 1..=2 execute the tool  (2 executions)
    //   - iteration 3 trips the doom guard pre-execution and terminates
    // ⇒ executions == total_llm_calls - 1 == 2.
    let dir = tempfile::tempdir().unwrap();
    let exec_count = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(CountingAlwaysNamedToolProvider {
        calls: AtomicUsize::new(0),
        tool_name: "loopy_tool",
    });
    let provider_arc: Arc<dyn LlmProvider> = provider.clone();
    let mut tools = ToolRegistry::with_builtins(dir.path());
    tools.register(CountingEchoTool {
        name: "loopy_tool",
        output: "looped",
        calls: exec_count.clone(),
    });
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
        .expect("process_message should return Ok even when the doom guard aborts");
    assert_eq!(result.content, doom_loop_terminal_message("loopy_tool", 3));

    let total_calls = provider.calls.load(AtomicOrdering::SeqCst);
    let executions = exec_count.load(AtomicOrdering::SeqCst);
    assert_eq!(total_calls, 3, "doom aborts before a 4th LLM call");
    assert_eq!(
        executions,
        total_calls - 1,
        "the tripping call must NOT execute; got {executions} executions \
             across {total_calls} LLM calls"
    );
}

#[test]
fn doom_loop_terminal_message_is_model_and_user_facing() {
    let msg = doom_loop_terminal_message("read_file", 3);
    assert!(msg.contains("read_file"), "names the looping tool: {msg}");
    assert!(msg.contains('3'), "states the streak length: {msg}");
    assert!(
        msg.contains("identical arguments"),
        "explains WHY the turn stopped: {msg}"
    );
    assert!(
        msg.contains("different approach") || msg.contains("rephrase"),
        "guides the model/user toward a way forward: {msg}"
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
        std::sync::Arc::new(crate::sandbox::NoSandbox),
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
        "ready workspace should not produce contract failure summary: {failures:?}"
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
        "summary should name the failing repo:\n{failures}"
    );
    assert!(
        failures.contains("completion failed") || failures.contains("artifact missing"),
        "summary should describe what failed:\n{failures}"
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
        "ready-deck should not appear as a failure:\n{failures}"
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
    let response = spawn_only_chat_response(vec![spawn_only_tool_call("call_1", "run_pipeline")]);
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
    let response = spawn_only_chat_response(vec![spawn_only_tool_call("call_now", "bg_research")]);
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
    let response = spawn_only_chat_response(vec![spawn_only_tool_call("call_pl", "deep_search")]);
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
    let response = spawn_only_chat_response(vec![spawn_only_tool_call("call_blocked", "shell")]);
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
        result.content, "Pipeline launched; shell-helper failed; read_task_output is clean — done.",
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
        result.content, "Pipeline launched; shell-helper failed and I cannot proceed without it.",
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

// ── Phase 4: human-approval suspend-and-resume (ROBRIX-PHASE4 ADR) ──────

struct AlphaToolThenEndProvider {
    calls: AtomicUsize,
}

#[async_trait]
impl LlmProvider for AlphaToolThenEndProvider {
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
                tool_calls: vec![ToolCall {
                    id: "call_alpha".to_string(),
                    name: "alpha".to_string(),
                    arguments: serde_json::json!({"value": "x"}),
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

fn human_approval_rules_for(tool: &str) -> crate::approval::HumanApprovalRules {
    crate::approval::HumanApprovalRules::new(vec![crate::approval::ApprovalRule {
        tools: vec![tool.to_string()],
        risk_level: crate::approval::ApprovalRiskLevel::Critical,
        authorized_approvers: vec!["@alice:example.org".to_string()],
        expires_in_secs: 300,
        on_timeout: crate::approval::ApprovalTimeoutBehavior::Notify,
    }])
}

#[tokio::test]
async fn should_suspend_turn_when_tool_matches_human_approval_rule() {
    let dir = tempfile::tempdir().unwrap();
    let mut tools = ToolRegistry::with_builtins(dir.path());
    tools.register(NamedEchoTool {
        name: "alpha",
        output: "alpha ok",
    });
    let provider: Arc<dyn LlmProvider> = Arc::new(AlphaToolThenEndProvider {
        calls: AtomicUsize::new(0),
    });
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("approval-test"), provider, tools, memory).with_config(
        AgentConfig {
            human_approval_rules: Some(human_approval_rules_for("alpha")),
            ..AgentConfig::default()
        },
    );

    let result = agent.process_message("do work", &[], vec![]).await.unwrap();

    let pending = result
        .pending_approval
        .expect("turn should suspend with a pending approval draft");
    assert_eq!(pending.request.tool_name, "alpha");
    assert_eq!(
        pending.request.authorized_approvers,
        vec!["@alice:example.org".to_string()]
    );
    assert!(result.content.is_empty());
    // The tool must NOT have executed.
    assert!(
        !result
            .messages
            .iter()
            .any(|m| m.content.contains("alpha ok")),
        "gated tool must not execute before approval"
    );
    // The placeholder tool result keeps the LLM history consistent.
    assert!(
        result
            .messages
            .iter()
            .any(|m| m.role == MessageRole::Tool && m.content.contains("[APPROVAL REQUESTED]")),
        "history should carry the approval placeholder: {:?}",
        result.messages
    );
}

#[tokio::test]
async fn should_execute_normally_when_no_human_approval_rule_matches() {
    let dir = tempfile::tempdir().unwrap();
    let mut tools = ToolRegistry::with_builtins(dir.path());
    tools.register(NamedEchoTool {
        name: "alpha",
        output: "alpha ok",
    });
    let provider: Arc<dyn LlmProvider> = Arc::new(AlphaToolThenEndProvider {
        calls: AtomicUsize::new(0),
    });
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("approval-test"), provider, tools, memory).with_config(
        AgentConfig {
            human_approval_rules: Some(human_approval_rules_for("beta")),
            ..AgentConfig::default()
        },
    );

    let result = agent.process_message("do work", &[], vec![]).await.unwrap();

    assert!(result.pending_approval.is_none());
    assert_eq!(result.content, "done");
    assert!(
        result
            .messages
            .iter()
            .any(|m| m.content.contains("alpha ok")),
        "non-matching tool should run normally"
    );
}

#[tokio::test]
async fn should_run_tool_directly_when_executing_approved_pending() {
    let dir = tempfile::tempdir().unwrap();
    let mut tools = ToolRegistry::with_builtins(dir.path());
    tools.register(NamedEchoTool {
        name: "alpha",
        output: "alpha ok",
    });
    let provider: Arc<dyn LlmProvider> = Arc::new(AlphaToolThenEndProvider {
        calls: AtomicUsize::new(0),
    });
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("approval-test"), provider, tools, memory);

    let pending = human_approval_rules_for("alpha")
        .draft_for_tool_call(
            "alpha",
            "call_alpha",
            serde_json::json!({"value": "x"}),
            chrono::Utc::now(),
        )
        .unwrap()
        .unwrap()
        .into_pending("!room:example.org", "@requester:example.org");

    let result = agent.execute_approved_tool(&pending).await.unwrap();
    assert!(result.success);
    assert_eq!(result.output, "alpha ok");
}

#[tokio::test]
async fn should_not_re_deny_approved_shell_command_tripping_safepolicy_ask() {
    // Codex/mempal review #1: an approved `shell` command that ALSO trips
    // SafePolicy's Decision::Ask (sudo / rm -rf / git push --force) must
    // not be re-denied by the in-tool approval gate. `execute_approved_tool`
    // scopes an auto-approver so the already-human-approved call runs.
    let dir = tempfile::tempdir().unwrap();
    let tools = ToolRegistry::with_builtins(dir.path());
    let provider: Arc<dyn LlmProvider> = Arc::new(AlphaToolThenEndProvider {
        calls: AtomicUsize::new(0),
    });
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("approval-shell"), provider, tools, memory);

    // `git push --force` trips SafePolicy::Ask; with no remote it fails
    // fast and harmlessly — we only assert the approval gate was passed.
    let pending = human_approval_rules_for("shell")
        .draft_for_tool_call(
            "shell",
            "call_shell",
            serde_json::json!({"command": "git push --force"}),
            chrono::Utc::now(),
        )
        .unwrap()
        .unwrap()
        .into_pending("!room:example.org", "@requester:example.org");

    let result = agent.execute_approved_tool(&pending).await.unwrap();
    assert!(
        !result.output.contains("requires approval"),
        "approved shell command must not be re-denied by the in-tool gate: {}",
        result.output
    );
}

#[tokio::test]
async fn should_reject_revalidation_when_sender_not_authorized() {
    let dir = tempfile::tempdir().unwrap();
    let tools = ToolRegistry::with_builtins(dir.path());
    let provider: Arc<dyn LlmProvider> = Arc::new(AlphaToolThenEndProvider {
        calls: AtomicUsize::new(0),
    });
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("approval-test"), provider, tools, memory);

    let pending = human_approval_rules_for("alpha")
        .draft_for_tool_call(
            "alpha",
            "call_alpha",
            serde_json::json!({"value": "x"}),
            chrono::Utc::now(),
        )
        .unwrap()
        .unwrap()
        .into_pending("!room:example.org", "@requester:example.org");

    let err = agent
        .revalidate_pending_approval(&pending, "@mallory:example.org")
        .await
        .unwrap_err();
    assert!(err.contains("not authorized"));

    assert!(
        agent
            .revalidate_pending_approval(&pending, "@alice:example.org")
            .await
            .is_ok()
    );
}

// ─────────────────────────────────────────────────────────────────────
// Task 8 — FailFast LLM bail: no empty-response 2nd call, classify-once
// TurnFailure projection, hook-deny exclusion.
// ─────────────────────────────────────────────────────────────────────

/// Provider that always returns an empty (retriable) response and counts
/// how many times `chat` is invoked. The default `chat_stream` routes to
/// `chat`, so the counter equals the number of LLM call attempts.
struct AlwaysEmptyProvider {
    chat_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl LlmProvider for AlwaysEmptyProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &ChatConfig,
    ) -> Result<ChatResponse> {
        self.chat_calls.fetch_add(1, AtomicOrdering::SeqCst);
        Ok(ChatResponse {
            content: Some(String::new()),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: LlmTokenUsage::default(),
            provider_index: None,
        })
    }

    fn model_id(&self) -> &str {
        "mock-empty"
    }

    fn provider_name(&self) -> &str {
        "mock-empty"
    }
}

/// Provider that always fails with a (typed) server-side error. A 5xx
/// ServerError classifies as a retriable LLM error without tripping the
/// agent's `1 << attempt` backoff under FailFast (retry_max = 0).
struct AlwaysErrorProvider {
    chat_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl LlmProvider for AlwaysErrorProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &ChatConfig,
    ) -> Result<ChatResponse> {
        self.chat_calls.fetch_add(1, AtomicOrdering::SeqCst);
        Err(LlmError::new(
            LlmErrorKind::ServerError { status: 503 },
            "provider unavailable",
        )
        .into())
    }

    fn model_id(&self) -> &str {
        "mock-error"
    }

    fn provider_name(&self) -> &str {
        "mock-error"
    }
}

fn task_for(instruction: &str, dir: &std::path::Path) -> Task {
    Task::new(
        TaskKind::Code {
            instruction: instruction.to_string(),
            files: vec![],
        },
        TaskContext {
            working_dir: dir.to_path_buf(),
            ..Default::default()
        },
    )
}

#[tokio::test]
async fn should_not_retry_empty_response_when_failfast() {
    use octos_llm::{LlmCallPolicy, with_llm_call_policy};

    let dir = tempfile::tempdir().unwrap();
    let chat_calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn LlmProvider> = Arc::new(AlwaysEmptyProvider {
        chat_calls: chat_calls.clone(),
    });
    let tools = ToolRegistry::with_builtins(dir.path());
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let mut agent = Agent::new(AgentId::new("ff-empty"), provider, tools, memory);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent.set_voice_failure_sink(tx);

    let _ = with_llm_call_policy(LlmCallPolicy::FailFast, async {
        agent.process_message("hi", &[], vec![]).await
    })
    .await;

    assert_eq!(
        chat_calls.load(AtomicOrdering::SeqCst),
        1,
        "FailFast empty response must NOT make an adaptive 2nd call"
    );
    let failure = rx.try_recv().expect("one TurnFailure emitted");
    assert!(
        matches!(failure, crate::TurnFailure::EmptyResponse),
        "expected EmptyResponse projection, got {failure:?}"
    );
    assert!(rx.try_recv().is_err(), "exactly one TurnFailure");
}

#[tokio::test]
async fn should_classify_once_and_emit_turn_failure_when_failfast_llm_error() {
    use octos_llm::{LlmCallPolicy, with_llm_call_policy};

    let dir = tempfile::tempdir().unwrap();
    let chat_calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn LlmProvider> = Arc::new(AlwaysErrorProvider {
        chat_calls: chat_calls.clone(),
    });
    let tools = ToolRegistry::with_builtins(dir.path());
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let mut agent = Agent::new(AgentId::new("ff-error"), provider, tools, memory);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent.set_voice_failure_sink(tx);

    let result = with_llm_call_policy(LlmCallPolicy::FailFast, async {
        agent.run_task(&task_for("hi", dir.path())).await
    })
    .await;

    // The ORIGINAL eyre::Report still bubbles out of the loop unchanged.
    assert!(result.is_err(), "FailFast LLM error must bail with Err");
    // Single foreground attempt — no adaptive/dispatch retry under FailFast.
    assert_eq!(
        chat_calls.load(AtomicOrdering::SeqCst),
        1,
        "FailFast LLM error must not retry"
    );
    // Exactly one TurnFailure::LlmError emitted (classify-once side effect
    // produced the classified HarnessError carried by the projection).
    let failure = rx.try_recv().expect("one TurnFailure emitted");
    assert!(
        matches!(failure, crate::TurnFailure::LlmError { .. }),
        "expected LlmError projection, got {failure:?}"
    );
    assert!(rx.try_recv().is_err(), "exactly one TurnFailure");
}

#[tokio::test]
#[cfg(unix)]
async fn should_not_emit_turn_failure_when_hook_denies_llm_call_under_failfast() {
    use crate::hooks::{HookConfig, HookEvent, HookExecutor};
    use octos_llm::{LlmCallPolicy, with_llm_call_policy};

    let dir = tempfile::tempdir().unwrap();
    let chat_calls = Arc::new(AtomicUsize::new(0));
    // Provider would error if reached, but the before_llm hook denies first.
    let provider: Arc<dyn LlmProvider> = Arc::new(AlwaysErrorProvider {
        chat_calls: chat_calls.clone(),
    });
    let tools = ToolRegistry::with_builtins(dir.path());
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    // `false` exits with code 1 → hook Deny → llm_call.rs bails with
    // "LLM call denied by hook: ..".
    let hooks = Arc::new(HookExecutor::new(vec![HookConfig {
        event: HookEvent::BeforeLlmCall,
        command: vec!["false".into()],
        timeout_ms: 5000,
        tool_filter: vec![],
        path_filter: vec![],
        requires_bin: None,
    }]));
    let mut agent =
        Agent::new(AgentId::new("ff-hookdeny"), provider, tools, memory).with_hooks(hooks);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent.set_voice_failure_sink(tx);

    let result = with_llm_call_policy(LlmCallPolicy::FailFast, async {
        agent.run_task(&task_for("hi", dir.path())).await
    })
    .await;

    assert!(result.is_err(), "hook-deny must still bail with Err");
    assert_eq!(
        chat_calls.load(AtomicOrdering::SeqCst),
        0,
        "hook denied the call before the provider was reached"
    );
    assert!(
        rx.try_recv().is_err(),
        "hook-deny must NOT emit a TurnFailure (preserve permission behaviour)"
    );
}

/// Records the message contents of every LLM call and returns EndTurn
/// immediately. Used to assert what the model actually saw and that it was
/// (or was not) called at all.
#[cfg(unix)]
struct RecordingEndProvider {
    chat_calls: Arc<AtomicUsize>,
    observed: Arc<StdMutex<Vec<Vec<String>>>>,
}

#[cfg(unix)]
#[async_trait]
impl LlmProvider for RecordingEndProvider {
    async fn chat(
        &self,
        messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &ChatConfig,
    ) -> Result<ChatResponse> {
        self.chat_calls.fetch_add(1, AtomicOrdering::SeqCst);
        self.observed
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(messages.iter().map(|m| m.content.clone()).collect());
        Ok(ChatResponse {
            content: Some("ok".to_string()),
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

#[tokio::test]
#[cfg(unix)]
async fn user_prompt_submit_hook_injects_stdout_as_turn_context() {
    use crate::hooks::{HookConfig, HookEvent, HookExecutor};

    let dir = tempfile::tempdir().unwrap();
    let tools = ToolRegistry::with_builtins(dir.path());
    let chat_calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(StdMutex::new(Vec::new()));
    let provider: Arc<dyn LlmProvider> = Arc::new(RecordingEndProvider {
        chat_calls: chat_calls.clone(),
        observed: Arc::clone(&observed),
    });
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    // Hook prints a context note on stdout (exit 0). It must be injected
    // into the model's input for this turn.
    let hooks = Arc::new(HookExecutor::new(vec![HookConfig {
        event: HookEvent::UserPromptSubmit,
        command: vec!["sh".into(), "-c".into(), "echo OCTOS_CTX_MARKER_42".into()],
        timeout_ms: 5000,
        tool_filter: vec![],
        path_filter: vec![],
        requires_bin: None,
    }]));
    let agent = Agent::new(AgentId::new("ups-inject"), provider, tools, memory).with_hooks(hooks);

    let result = agent.process_message("do work", &[], vec![]).await.unwrap();
    assert_eq!(result.content, "ok");
    assert_eq!(chat_calls.load(AtomicOrdering::SeqCst), 1);

    // The injected context reaches the model input for this turn...
    let prompts = observed.lock().unwrap_or_else(|error| error.into_inner());
    assert_eq!(prompts.len(), 1, "exactly one LLM call");
    assert!(
        prompts[0]
            .iter()
            .any(|content| content.contains("OCTOS_CTX_MARKER_42")),
        "injected context must reach the model input; got {:?}",
        prompts[0]
    );
    // ...but is NOT persisted as a conversation message.
    assert!(
        result
            .messages
            .iter()
            .all(|m| !m.content.contains("OCTOS_CTX_MARKER_42")),
        "injected context must not be persisted as a message"
    );
}

#[tokio::test]
#[cfg(unix)]
async fn user_prompt_submit_hook_deny_blocks_turn_before_llm() {
    use crate::hooks::{HookConfig, HookEvent, HookExecutor};

    let dir = tempfile::tempdir().unwrap();
    let tools = ToolRegistry::with_builtins(dir.path());
    let chat_calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(StdMutex::new(Vec::new()));
    let provider: Arc<dyn LlmProvider> = Arc::new(RecordingEndProvider {
        chat_calls: chat_calls.clone(),
        observed: Arc::clone(&observed),
    });
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    // Hook writes its reason on stdout and exits 1 → prompt denied.
    let hooks = Arc::new(HookExecutor::new(vec![HookConfig {
        event: HookEvent::UserPromptSubmit,
        command: vec![
            "sh".into(),
            "-c".into(),
            "echo 'no coding on fridays'; exit 1".into(),
        ],
        timeout_ms: 5000,
        tool_filter: vec![],
        path_filter: vec![],
        requires_bin: None,
    }]));
    let agent = Agent::new(AgentId::new("ups-deny"), provider, tools, memory).with_hooks(hooks);

    let result = agent
        .process_message("write code", &[], vec![])
        .await
        .unwrap();

    // The turn is blocked and the hook's reason is surfaced...
    assert!(
        result.content.contains("[HOOK DENIED]"),
        "deny should be clearly surfaced; got {:?}",
        result.content
    );
    assert!(
        result.content.contains("no coding on fridays"),
        "deny reason (hook stdout) should be surfaced; got {:?}",
        result.content
    );
    // ...and the LLM is never reached.
    assert_eq!(
        chat_calls.load(AtomicOrdering::SeqCst),
        0,
        "denied prompt must not reach the model"
    );
    assert!(
        observed
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_empty()
    );
}

// --- Mid-turn steer injection (codex `TurnState.pending_input` parity) ---

/// Per-request prompt capture: `(role, content)` per message, one vec per
/// LLM call.
type ObservedRolePrompts = Arc<StdMutex<Vec<Vec<(MessageRole, String)>>>>;

/// Records every prompt as `(role, content)` pairs. Call 0 pushes the given
/// steer inputs into the shared buffer MID-CALL (simulating a `turn/steer`
/// racing in while the model streams), then returns a tool call; call 1
/// returns EndTurn.
struct SteerDuringToolRoundProvider {
    calls: AtomicUsize,
    observed: ObservedRolePrompts,
    buffer: crate::steering::SharedSteerBuffer,
    steers: Vec<String>,
}

#[async_trait]
impl LlmProvider for SteerDuringToolRoundProvider {
    async fn chat(
        &self,
        messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> Result<ChatResponse> {
        self.observed
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(
                messages
                    .iter()
                    .map(|message| (message.role, message.content.clone()))
                    .collect(),
            );
        let call = self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        Ok(if call == 0 {
            for steer in &self.steers {
                self.buffer.push(steer.clone());
            }
            tool_use(
                vec![ToolCall {
                    id: "call_alpha".to_string(),
                    name: "alpha".to_string(),
                    arguments: serde_json::json!({}),
                    metadata: None,
                }],
                1,
                1,
            )
        } else {
            end_turn("done", 1, 1)
        })
    }

    fn model_id(&self) -> &str {
        "mock"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

/// Call 0 pushes steer inputs mid-call and returns a FINAL answer
/// (EndTurn). The pending steer must force one more round; call 1 returns
/// the follow-up answer.
struct SteerAfterFinalAnswerProvider {
    calls: AtomicUsize,
    observed: ObservedRolePrompts,
    buffer: crate::steering::SharedSteerBuffer,
    steers: Vec<String>,
}

#[async_trait]
impl LlmProvider for SteerAfterFinalAnswerProvider {
    async fn chat(
        &self,
        messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> Result<ChatResponse> {
        self.observed
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(
                messages
                    .iter()
                    .map(|message| (message.role, message.content.clone()))
                    .collect(),
            );
        let call = self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        Ok(if call == 0 {
            for steer in &self.steers {
                self.buffer.push(steer.clone());
            }
            end_turn("first answer", 1, 1)
        } else {
            end_turn("second answer", 1, 1)
        })
    }

    fn model_id(&self) -> &str {
        "mock"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

/// Steer landing between tool rounds is drained at the top of the next
/// iteration, BEFORE the next LLM call, as a plain `role: user` message
/// appended after the previous round's tool output — no wrapper text.
#[tokio::test]
async fn should_fold_steer_into_next_llm_call_when_injected_between_rounds() {
    let dir = tempfile::tempdir().unwrap();
    let mut tools = ToolRegistry::with_builtins(dir.path());
    tools.register(NamedEchoTool {
        name: "alpha",
        output: "alpha ok",
    });
    let buffer: crate::steering::SharedSteerBuffer =
        Arc::new(crate::steering::SteerBuffer::default());
    let observed = Arc::new(StdMutex::new(Vec::new()));
    let provider: Arc<dyn LlmProvider> = Arc::new(SteerDuringToolRoundProvider {
        calls: AtomicUsize::new(0),
        observed: Arc::clone(&observed),
        buffer: Arc::clone(&buffer),
        steers: vec!["also check the tests".to_string()],
    });
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("steer-agent"), provider, tools, memory)
        .with_steer_buffer(Arc::clone(&buffer));

    let result = agent.process_message("do work", &[], vec![]).await.unwrap();
    assert_eq!(result.content, "done");

    let observed = observed.lock().unwrap_or_else(|error| error.into_inner());
    assert_eq!(observed.len(), 2, "tool round + follow-up round");
    // First call: no steer yet (it lands mid-call).
    assert!(
        !observed[0]
            .iter()
            .any(|(_, content)| content.contains("also check the tests")),
        "steer must not time-travel into the request that was already built"
    );
    // Second call: the steer is a plain user message with NO wrapper text,
    // appended AFTER the previous round's tool output.
    let steer_idx = observed[1]
        .iter()
        .position(|(role, content)| *role == MessageRole::User && content == "also check the tests")
        .expect("second request must carry the steer as a plain role:user message");
    let tool_idx = observed[1]
        .iter()
        .position(|(role, _)| *role == MessageRole::Tool)
        .expect("second request must carry the tool result");
    assert!(
        steer_idx > tool_idx,
        "steer must append after the prior round's tool output (append-only history)"
    );
    // Buffer fully drained.
    assert!(buffer.is_empty());
    // No callback registered → the steer row rides the turn output log so
    // the host's end-of-turn persistence writes it exactly once.
    assert!(
        result
            .messages
            .iter()
            .any(|m| m.role == MessageRole::User && m.content == "also check the tests"),
        "without a drained-callback the steer row must land in the turn output log"
    );
}

/// A steer that lands while the model produces its FINAL answer forces one
/// more round (`needs_follow_up = model_wants_more || buffer_nonempty`),
/// and the prior answer is recorded as a normal assistant row the model
/// sees in the follow-up request.
#[tokio::test]
async fn should_run_extra_round_when_steer_lands_after_final_answer() {
    let dir = tempfile::tempdir().unwrap();
    let tools = ToolRegistry::with_builtins(dir.path());
    let buffer: crate::steering::SharedSteerBuffer =
        Arc::new(crate::steering::SteerBuffer::default());
    let observed = Arc::new(StdMutex::new(Vec::new()));
    let provider: Arc<dyn LlmProvider> = Arc::new(SteerAfterFinalAnswerProvider {
        calls: AtomicUsize::new(0),
        observed: Arc::clone(&observed),
        buffer: Arc::clone(&buffer),
        steers: vec!["one more thing".to_string()],
    });
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("steer-agent"), provider, tools, memory)
        .with_steer_buffer(Arc::clone(&buffer));

    let result = agent.process_message("do work", &[], vec![]).await.unwrap();

    // The steer forced a second round and the turn ends on ITS answer.
    assert_eq!(result.content, "second answer");
    let first_index = result
        .messages
        .iter()
        .position(|message| {
            message.role == MessageRole::Assistant && message.content == "first answer"
        })
        .unwrap();
    assert_eq!(
        result.assistant_segments.message_iterations,
        vec![(first_index, 1)],
        "the tool-free pre-steer answer has its own producer identity"
    );
    assert_eq!(result.assistant_segments.final_iteration, 2);
    let observed = observed.lock().unwrap_or_else(|error| error.into_inner());
    assert_eq!(
        observed.len(),
        2,
        "steer after EndTurn must force another round"
    );
    let assistant_idx = observed[1]
        .iter()
        .position(|(role, content)| *role == MessageRole::Assistant && content == "first answer")
        .expect("follow-up request must carry the recorded first answer");
    let steer_idx = observed[1]
        .iter()
        .position(|(role, content)| *role == MessageRole::User && content == "one more thing")
        .expect("follow-up request must carry the steer as a plain role:user message");
    assert!(
        steer_idx > assistant_idx,
        "steer must follow the recorded final answer in history order"
    );
    // Both the intermediate answer and the steer row persist via the log.
    assert!(
        result
            .messages
            .iter()
            .any(|m| m.role == MessageRole::Assistant && m.content == "first answer"),
        "the pre-steer final answer must persist as a normal assistant row"
    );
    assert!(
        result
            .messages
            .iter()
            .any(|m| m.role == MessageRole::User && m.content == "one more thing")
    );
}

/// Rapid steers drain together, in FIFO order, at the next boundary.
#[tokio::test]
async fn should_preserve_fifo_order_when_multiple_steers_accumulate() {
    let dir = tempfile::tempdir().unwrap();
    let tools = ToolRegistry::with_builtins(dir.path());
    let buffer: crate::steering::SharedSteerBuffer =
        Arc::new(crate::steering::SteerBuffer::default());
    let observed = Arc::new(StdMutex::new(Vec::new()));
    let provider: Arc<dyn LlmProvider> = Arc::new(SteerAfterFinalAnswerProvider {
        calls: AtomicUsize::new(0),
        observed: Arc::clone(&observed),
        buffer: Arc::clone(&buffer),
        steers: vec!["steer one".to_string(), "steer two".to_string()],
    });
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("steer-agent"), provider, tools, memory)
        .with_steer_buffer(Arc::clone(&buffer));

    let result = agent.process_message("do work", &[], vec![]).await.unwrap();
    assert_eq!(result.content, "second answer");

    let observed = observed.lock().unwrap_or_else(|error| error.into_inner());
    assert_eq!(observed.len(), 2);
    let first_idx = observed[1]
        .iter()
        .position(|(role, content)| *role == MessageRole::User && content == "steer one")
        .expect("first steer present");
    let second_idx = observed[1]
        .iter()
        .position(|(role, content)| *role == MessageRole::User && content == "steer two")
        .expect("second steer present");
    assert!(
        first_idx < second_idx,
        "steers must drain in FIFO (arrival) order"
    );
}

/// The drained-callback is a live hook (the host may echo the steer to a
/// client); it never owns persistence. The steer stays in the chronological
/// turn output log at its model-visible position — after the answer it
/// followed — so the end-of-turn persist writes durable rows in the order
/// the model saw them and a context ledger rebuilt from that history keeps
/// the chronology. The prompt carries it too.
#[tokio::test]
async fn should_hand_drained_steers_to_callback_and_keep_them_in_output_log_order() {
    let dir = tempfile::tempdir().unwrap();
    let tools = ToolRegistry::with_builtins(dir.path());
    let buffer: crate::steering::SharedSteerBuffer =
        Arc::new(crate::steering::SteerBuffer::default());
    let observed = Arc::new(StdMutex::new(Vec::new()));
    let provider: Arc<dyn LlmProvider> = Arc::new(SteerAfterFinalAnswerProvider {
        calls: AtomicUsize::new(0),
        observed: Arc::clone(&observed),
        buffer: Arc::clone(&buffer),
        steers: vec!["persist me host-side".to_string()],
    });
    let drained_batches: Arc<StdMutex<Vec<Vec<String>>>> = Arc::new(StdMutex::new(Vec::new()));
    let drained_for_callback = Arc::clone(&drained_batches);
    let callback: crate::steering::SteerDrainedCallback = Arc::new(move |batch: Vec<String>| {
        let drained = Arc::clone(&drained_for_callback);
        Box::pin(async move {
            drained
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(batch);
        })
    });
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("steer-agent"), provider, tools, memory)
        .with_steer_buffer(Arc::clone(&buffer))
        .with_steer_drained_callback(callback);

    let result = agent.process_message("do work", &[], vec![]).await.unwrap();
    assert_eq!(result.content, "second answer");

    let batches = drained_batches
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    assert_eq!(
        batches.as_slice(),
        [vec!["persist me host-side".to_string()]]
    );
    // The durable log carries the steer exactly once, AFTER the first answer
    // it was injected behind (chronological persistence is what lets a
    // rebuild from session history reproduce the model-visible order).
    let steer_rows = result
        .messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == MessageRole::User && m.content == "persist me host-side")
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    assert_eq!(steer_rows.len(), 1, "log: {:?}", result.messages);
    let first_answer = result
        .messages
        .iter()
        .position(|m| m.role == MessageRole::Assistant)
        .expect("first answer in the durable log");
    assert!(
        steer_rows[0] > first_answer,
        "the steer must follow the answer it was injected behind: {:?}",
        result.messages
    );
    // ...and the model did see it.
    let observed = observed.lock().unwrap_or_else(|error| error.into_inner());
    assert!(
        observed[1]
            .iter()
            .any(|(role, content)| *role == MessageRole::User && content == "persist me host-side")
    );
}

/// Assistant shell call + its Tool result, as one exchange (spiral-signature
/// port, spec kv-cache era fixes 2026-08-03).
fn spiral_shell_exchange(id: &str, command: &str, output: &str) -> [Message; 2] {
    [
        Message {
            role: MessageRole::Assistant,
            content: String::new(),
            media: vec![],
            tool_calls: Some(vec![ToolCall {
                id: id.into(),
                name: "shell".into(),
                arguments: serde_json::json!({"command": command}),
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
            content: output.into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: Some(id.into()),
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        },
    ]
}

#[test]
fn distinct_failing_exploration_commands_are_not_a_retry_spiral() {
    // Observed live (2026-08-02, kimi k3): agentic models fan out many
    // DIFFERENT exploratory commands per turn, several exiting non-zero
    // (grep with no match exits 1, ls on a guessed path fails). The
    // RetryLimit arm counted ANY >=3 failures as a spiral and killed
    // legitimate exploration turns mid-task. A retry spiral means the
    // SAME command failing over and over — distinct failures are work,
    // not a loop, and must not trip it.
    let mut messages = vec![Message::user("analyze the makepad repo")];
    // The grep outputs model the REAL shell tool: a no-match grep prints
    // nothing, and the tool renders empty output as "(no output)" plus the
    // exit-code suffix (shell.rs), not as a bare "Exit code: 1".
    messages.extend(spiral_shell_exchange(
        "call_1",
        "grep -rn 'Widget' src/",
        "(no output)\n\nExit code: 1",
    ));
    messages.extend(spiral_shell_exchange(
        "call_2",
        "ls examples/aichat",
        "No such file or directory\n\nExit code: 1",
    ));
    messages.extend(spiral_shell_exchange(
        "call_3",
        "grep -rn 'live_design' docs/",
        "(no output)\n\nExit code: 1",
    ));
    messages.extend(spiral_shell_exchange(
        "call_4",
        "cat platform/README.md",
        "No such file\n\nExit code: 1",
    ));

    assert!(
        recover_shell_retry(&messages, 4).is_none(),
        "distinct failing commands are exploration, not a retry spiral"
    );
}

#[test]
fn distinct_no_output_failures_are_not_a_retry_spiral() {
    // The commonest exploration pattern: several DISTINCT grep/rg probes that
    // all match nothing. The shell tool renders each as literally
    // "(no output)\n\nExit code: 1", so after the exit-code suffix is stripped
    // every one of them shares the SAME failure text — the tool's "(no output)"
    // sentinel. If that sentinel is counted as a failure signature, three
    // no-match greps "concentrate on one repeated failure text" and the
    // RetryLimit arm kills a healthy exploration turn — the exact false
    // positive the signature gate exists to prevent. "(no output)" means the
    // command produced no error text at all, so it must count as EMPTY.
    let mut messages = vec![Message::user("where is the onboarding flow?")];
    for (id, command) in [
        ("call_1", "grep -rn 'onboarding' src/"),
        ("call_2", "grep -rn 'Onboarding' crates/"),
        ("call_3", "rg -l 'welcome_screen'"),
        ("call_4", "grep -rn 'first_run' app/"),
    ] {
        messages.extend(spiral_shell_exchange(
            id,
            command,
            "(no output)\n\nExit code: 1",
        ));
    }

    assert!(
        recover_shell_retry(&messages, 4).is_none(),
        "distinct no-match probes share the (no output) sentinel, not a failure \
         text — they must not trip the retry limit"
    );
}

#[test]
fn repeated_identical_failing_command_still_trips_the_retry_limit() {
    // The case the detector exists for: the model re-runs the same failing
    // command without converging.
    let mut messages = vec![Message::user("fetch the news")];
    for id in ["call_1", "call_2", "call_3", "call_4"] {
        messages.extend(spiral_shell_exchange(
            id,
            "curl -s https://news.example/api",
            "curl: (28) Connection timed out\n\nExit code: 28",
        ));
    }

    let recovery =
        recover_shell_retry(&messages, 4).expect("the same command failing repeatedly is a spiral");
    assert!(matches!(recovery.kind, ShellRetryRecoveryKind::RetryLimit));
}

/// Append-only measurement, end to end through the real loop.
///
/// The point of the audit is to answer one question with evidence rather than
/// argument: does octos already rewrite request history in place? This drives
/// two real turns on one agent — the second carrying the first's oversized
/// tool result as history — which is the shape `truncate_old_tool_results`
/// acts on, and asserts the audit both RAN and reported it.
///
/// The `RAN` half matters as much as the finding. An earlier version of this
/// measurement reported nothing, and the nothing meant only that octos-agent's
/// lib tests install no `tracing` subscriber, so every `warn!` went nowhere.
/// A measurement whose silence cannot be distinguished from absence is not a
/// measurement, so findings are recorded out-of-band and asserted here.
#[tokio::test]
async fn append_only_audit_observes_the_in_place_truncation_across_turns() {
    crate::agent::append_only_audit::arm_for_test();
    let _ = crate::agent::append_only_audit::drain_findings();
    let before = crate::agent::append_only_audit::finding_count();

    let dir = tempfile::tempdir().unwrap();
    let mut tools = ToolRegistry::with_builtins(dir.path());
    // Comfortably past the 800-char collapse threshold.
    tools.register(NamedEchoTool {
        name: "alpha",
        output: BIG_TOOL_OUTPUT,
    });

    let provider: Arc<dyn LlmProvider> = Arc::new(MultiToolThenEndProvider {
        calls: AtomicUsize::new(0),
    });
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("append-only-audit"), provider, tools, memory);

    let first = agent.process_message("do work", &[], vec![]).await.unwrap();
    // Second turn carries the first turn's tool results as history, which is
    // what puts them BEFORE the newest user message.
    let _second = agent
        .process_message("and now the next thing", &first.messages, vec![])
        .await
        .unwrap();

    let after = crate::agent::append_only_audit::finding_count();
    let findings = crate::agent::append_only_audit::drain_findings();
    crate::agent::append_only_audit::disarm_for_test();

    assert!(
        after > before,
        "the audit must have RUN; a silent result here means the wiring is dead, \
         not that octos is append-only (findings: {findings:?})"
    );
    assert!(
        findings.iter().any(|f| f.contains("rewritten in place")),
        "expected an in-place rewrite across turns; got {findings:?}"
    );
}

/// Large enough that `truncate_old_tool_results` collapses it.
const BIG_TOOL_OUTPUT: &str = concat!(
    "BEGIN-LARGE-TOOL-RESULT ",
    include_str!("append_only_audit.rs"),
);

/// A tool whose output overflows the cap and which knows how to resume.
struct OverflowingPagedTool;

#[async_trait]
impl Tool for OverflowingPagedTool {
    fn name(&self) -> &str {
        "overflowing_paged"
    }

    fn description(&self) -> &str {
        "Return more output than the per-tool cap allows"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: &serde_json::Value) -> Result<ToolResult> {
        Ok(ToolResult {
            // Comfortably past the 50_000-byte default cap.
            output: "y".repeat(120_000),
            success: true,
            ..Default::default()
        })
    }

    fn truncation_recovery(
        &self,
        args: &serde_json::Value,
        omitted_bytes: usize,
    ) -> Option<String> {
        let page = args
            .get("page")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        Some(format!(
            "[{omitted_bytes} bytes omitted] Continue with page: {}.",
            page + 1
        ))
    }
}

struct CallsOverflowingToolThenEnds {
    calls: AtomicUsize,
}

#[async_trait]
impl LlmProvider for CallsOverflowingToolThenEnds {
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
                tool_calls: vec![ToolCall {
                    id: "call_overflow".to_string(),
                    name: "overflowing_paged".to_string(),
                    arguments: serde_json::json!({ "page": 0 }),
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
        "test-model"
    }

    fn provider_name(&self) -> &str {
        "test-provider"
    }
}

/// The wiring test: a truncated tool result must reach the model carrying its
/// recovery advice.
///
/// The unit tests prove `truncation_recovery` returns good text. They prove
/// nothing about whether the execution loop ever CALLS it — and an unwired
/// hook that returns perfect advice into the void is the failure mode this
/// whole change exists to remove. So this drives the real loop and inspects
/// the tool message the model actually received.
#[tokio::test]
async fn truncated_tool_output_reaches_the_model_with_its_recovery_advice() {
    let dir = tempfile::tempdir().unwrap();
    let mut tools = ToolRegistry::with_builtins(dir.path());
    tools.register(OverflowingPagedTool);

    let provider: Arc<dyn LlmProvider> = Arc::new(CallsOverflowingToolThenEnds {
        calls: AtomicUsize::new(0),
    });
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("truncation-recovery"), provider, tools, memory);

    let result = agent.process_message("go", &[], vec![]).await.unwrap();

    let tool_message = result
        .messages
        .iter()
        .find(|m| m.role == MessageRole::Tool)
        .expect("the turn must contain the tool result");

    assert!(
        tool_message.content.len() < 120_000,
        "the cap must still apply: got {} bytes",
        tool_message.content.len()
    );
    assert!(
        tool_message.content.contains("Continue with page: 1."),
        "the truncated result must carry the tool's recovery advice, otherwise the model is \
         left at a dead end and can only re-run the same call; tail was: {:?}",
        &tool_message.content[tool_message.content.len().saturating_sub(200)..]
    );
}

/// Every `<digits> bytes omitted` count in `content`, in order of appearance.
fn omitted_byte_counts(content: &str) -> Vec<u64> {
    let mut counts = Vec::new();
    for (idx, _) in content.match_indices(" bytes omitted") {
        let digits: Vec<char> = content[..idx]
            .chars()
            .rev()
            .take_while(char::is_ascii_digit)
            .collect();
        let digits: String = digits.into_iter().rev().collect();
        if !digits.is_empty() {
            counts.push(digits.parse().expect("ascii digits"));
        }
    }
    counts
}

/// The backstop's inline marker (`... [N bytes omitted] ...`) and the
/// recovery advice the tool composes (`[N bytes omitted] Continue...`) are
/// two tellings of the SAME cut and must carry the same byte count.
///
/// The legacy loop recomputed the recovery count as
/// `untruncated_len - content.len()`, which undercounts by the length of the
/// elision marker itself — the model was shown two numbers that never agreed.
/// The structured `truncate_head_tail_report` hands both consumers the one
/// `omitted_bytes` the split actually measured.
#[tokio::test]
async fn should_agree_on_omitted_bytes_between_marker_and_recovery_when_backstop_truncates() {
    let dir = tempfile::tempdir().unwrap();
    let mut tools = ToolRegistry::with_builtins(dir.path());
    tools.register(OverflowingPagedTool);

    let provider: Arc<dyn LlmProvider> = Arc::new(CallsOverflowingToolThenEnds {
        calls: AtomicUsize::new(0),
    });
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("truncation-agree"), provider, tools, memory);

    let result = agent.process_message("go", &[], vec![]).await.unwrap();
    let tool_message = result
        .messages
        .iter()
        .find(|m| m.role == MessageRole::Tool)
        .expect("the turn must contain the tool result");

    let counts = omitted_byte_counts(&tool_message.content);
    assert_eq!(
        counts.len(),
        2,
        "expected the elision marker and the recovery advice to each name a count; tail: {:?}",
        &tool_message.content[tool_message.content.len().saturating_sub(300)..]
    );
    assert_eq!(
        counts[0], counts[1],
        "marker and recovery advice must agree on how much was omitted"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// #27d (R4) — malformed tool-call feedback buffer
// ─────────────────────────────────────────────────────────────────────────

/// A provider whose first N `chat` calls fail with
/// `StreamError::MalformedArgs` and whose subsequent calls succeed with the
/// scripted response — models "the model emitted broken tool-call JSON, then
/// self-corrected after seeing the diagnostic".
struct MalformedThenOkProvider {
    malformed_first: StdMutex<usize>,
    ok_response: ChatResponse,
}

#[async_trait]
impl LlmProvider for MalformedThenOkProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &ChatConfig,
    ) -> Result<ChatResponse> {
        let mut guard = self
            .malformed_first
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if *guard > 0 {
            *guard -= 1;
            return Err(eyre::Report::new(octos_llm::StreamError::MalformedArgs {
                tool_id: "call_bad".to_string(),
                tool_name: "shell".to_string(),
                error: "expected `,` or `}` at line 1 column 4123".to_string(),
            }));
        }
        Ok(self.ok_response.clone())
    }

    fn model_id(&self) -> &str {
        "malformed-then-ok"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

fn plain_text_response(content: &str) -> ChatResponse {
    ChatResponse {
        content: Some(content.to_owned()),
        reasoning_content: None,
        tool_calls: Vec::new(),
        stop_reason: octos_llm::StopReason::EndTurn,
        usage: octos_llm::TokenUsage {
            input_tokens: 5,
            output_tokens: 5,
            ..Default::default()
        },
        provider_index: None,
    }
}

/// #27d — a MalformedArgs failure is fed back as a diagnostic message; the
/// model self-corrects on the next call and the TURN SURVIVES (pre-#27d the
/// same stream error terminated the turn instantly).
#[tokio::test]
async fn malformed_toolcall_feedback_lets_model_self_correct_and_survive() {
    let provider: Arc<dyn LlmProvider> = Arc::new(MalformedThenOkProvider {
        malformed_first: StdMutex::new(1),
        ok_response: plain_text_response("recovered: valid tool call emitted"),
    });
    let tools = ToolRegistry::new();
    let dir = tempfile::tempdir().unwrap();
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("malformed-feedback"), provider, tools, memory);
    let response = agent
        .process_message("do the thing with a tool", &[], vec![])
        .await
        .expect("turn survives the malformed tool call after feedback");
    assert_eq!(
        response.content, "recovered: valid tool call emitted",
        "the model's post-correction reply is the turn's answer"
    );
}

/// #27d — after MALFORMED_TOOLCALL_FEEDBACK_LIMIT (3) fed-back diagnostics
/// the buffer is exhausted and the turn terminates with the error (the
/// pinned pre-#27d behavior).
#[tokio::test]
async fn malformed_toolcall_feedback_exhausts_and_terminates() {
    let provider: Arc<dyn LlmProvider> = Arc::new(MalformedThenOkProvider {
        malformed_first: StdMutex::new(10), // never self-corrects
        ok_response: plain_text_response("unreachable"),
    });
    let tools = ToolRegistry::new();
    let dir = tempfile::tempdir().unwrap();
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("malformed-exhaust"), provider, tools, memory);
    let result = agent
        .process_message("never produces valid JSON", &[], vec![])
        .await;
    let err = result.expect_err("exhausted malformed budget terminates the turn");
    assert!(
        err.to_string().contains("MalformedArgs")
            || err.to_string().contains("malformed")
            || err.to_string().contains("arguments"),
        "the terminal error names the malformed-args failure: {err}"
    );
}

// --- build_chat_config: temperature/max_tokens override semantics (#2172) ---

#[test]
fn build_chat_config_keeps_default_temperature_when_unset() {
    // Cloud-safety invariant: with no chat_temperature override, the chat
    // temperature must remain the built-in ChatConfig default (0.0), so cloud
    // requests are byte-for-byte unchanged.
    let cfg = AgentConfig {
        chat_temperature: None,
        ..AgentConfig::default()
    };
    let chat = build_chat_config(&cfg, false);
    assert_eq!(chat.temperature, ChatConfig::default().temperature);
    assert_eq!(chat.temperature, Some(0.0));
}

#[test]
fn build_chat_config_applies_temperature_override() {
    let cfg = AgentConfig {
        chat_temperature: Some(0.7),
        ..AgentConfig::default()
    };
    let chat = build_chat_config(&cfg, false);
    assert_eq!(chat.temperature, Some(0.7));
}

#[test]
fn build_chat_config_threads_sampling_params() {
    // #2172: chat_sampling_params flows into ChatConfig.sampling_params; unset
    // → None (cloud unchanged).
    let mut sp = serde_json::Map::new();
    sp.insert("repeat_penalty".to_string(), serde_json::json!(1.1));
    let cfg = AgentConfig {
        chat_sampling_params: Some(sp),
        ..AgentConfig::default()
    };
    let chat = build_chat_config(&cfg, false);
    assert_eq!(
        chat.sampling_params
            .as_ref()
            .and_then(|m| m.get("repeat_penalty")),
        Some(&serde_json::json!(1.1))
    );
    assert_eq!(
        build_chat_config(&AgentConfig::default(), false).sampling_params,
        None
    );
}

#[test]
fn build_chat_config_applies_max_tokens_override_independently() {
    // Overrides compose without clobbering each other.
    let cfg = AgentConfig {
        chat_max_tokens: Some(4096),
        chat_temperature: Some(0.5),
        ..AgentConfig::default()
    };
    let chat = build_chat_config(&cfg, false);
    assert_eq!(chat.max_tokens, Some(4096));
    assert_eq!(chat.temperature, Some(0.5));
}

#[test]
fn build_chat_config_local_provider_unsets_temperature() {
    // #2229: on a local provider with no explicit chat_temperature, temperature
    // is left UNSET (None) so the server samples — the request omits it — rather
    // than forcing greedy 0.0 (which degenerates local reasoning models).
    let cfg = AgentConfig {
        chat_temperature: None,
        ..AgentConfig::default()
    };
    let chat = build_chat_config(&cfg, true);
    assert_eq!(chat.temperature, None);
    // Cloud path is unchanged: still the built-in 0.0.
    assert_eq!(build_chat_config(&cfg, false).temperature, Some(0.0));
}

#[test]
fn build_chat_config_local_provider_respects_explicit_temperature() {
    // An explicit override always wins, even on local.
    let cfg = AgentConfig {
        chat_temperature: Some(0.6),
        ..AgentConfig::default()
    };
    assert_eq!(build_chat_config(&cfg, true).temperature, Some(0.6));
}

// --- #2174: conversation-loop recovery from a degenerate empty MaxTokens ---

fn empty_max_tokens_response() -> ChatResponse {
    // Models the REAL degenerate case: the whole output budget was spent on
    // reasoning, so `content` is empty and there are no tool calls. Private
    // reasoning is not an answer: terminal-integrity retries treat this as
    // empty and eventually return an explicit error if no answer arrives.
    ChatResponse {
        content: None,
        reasoning_content: Some("(long internal reasoning, no final answer)".to_string()),
        tool_calls: vec![],
        stop_reason: StopReason::MaxTokens,
        usage: LlmTokenUsage {
            input_tokens: 5,
            output_tokens: 128,
            ..Default::default()
        },
        provider_index: None,
    }
}

async fn run_conversation_response(responses: Vec<ChatResponse>) -> Result<ConversationResponse> {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(responses));
    let tools = ToolRegistry::new();
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("empty-maxtokens"), provider, tools, memory).with_config(
        AgentConfig {
            max_iterations: 10,
            save_episodes: false,
            ..Default::default()
        },
    );
    agent.process_message("go", &[], vec![]).await
}

#[tokio::test]
async fn empty_max_tokens_recovers_when_retry_succeeds() {
    // A degenerate empty MaxTokens (no content, no tool call) must trigger a
    // nudge-and-retry instead of returning empty; the retry succeeds and its
    // content is returned — not a silent empty exit.
    let response = run_conversation_response(vec![
        empty_max_tokens_response(),
        end_turn("recovered answer", 4, 6),
    ])
    .await
    .unwrap();
    assert_eq!(response.content, "recovered answer");
}

/// Always returns the degenerate empty-MaxTokens response, so the loop's
/// behavior is bounded solely by the recovery cap (not by a fixed script).
struct AlwaysEmptyMaxTokensProvider(Arc<std::sync::atomic::AtomicUsize>);

#[async_trait]
impl LlmProvider for AlwaysEmptyMaxTokensProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &ChatConfig,
    ) -> Result<ChatResponse> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(empty_max_tokens_response())
    }
    fn model_id(&self) -> &str {
        "always-empty"
    }
    fn provider_name(&self) -> &str {
        "mock"
    }
}

#[tokio::test]
async fn empty_max_tokens_surfaces_error_after_recovery_exhausted() {
    // A model that keeps returning a degenerate empty MaxTokens: the loop does
    // two bounded recoveries then surfaces a clear terminal error rather than a
    // silent empty return. If the recovery counter did NOT persist across
    // iterations this would instead loop until max_iterations — so this also
    // pins the bound.
    let dir = tempfile::tempdir().unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let provider = Arc::new(AlwaysEmptyMaxTokensProvider(calls.clone()));
    let tools = ToolRegistry::new();
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("empty-maxtokens"), provider, tools, memory).with_config(
        AgentConfig {
            max_iterations: 10,
            save_episodes: false,
            ..Default::default()
        },
    );
    // Uses the default (non-FailFast) call policy — the path `octos chat`
    // takes, where an empty-but-reasoning MaxTokens reaches the conversation
    // loop rather than being failed fast. (Under FailFast the empty response is
    // terminal earlier, a different — also non-silent — outcome.)
    let error = agent.process_message("go", &[], vec![]).await.unwrap_err();
    assert!(
        error.to_string().contains("empty response after"),
        "{error:#}"
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::Relaxed),
        10,
        "two call-level recovery rounds must stay bounded"
    );
}

#[tokio::test]
async fn non_empty_max_tokens_preserves_partial_content_without_claiming_success() {
    // Real partial text is retained without retrying or relabeling truncation
    // as a successfully completed turn.
    let error = run_conversation_response(vec![ChatResponse {
        content: Some("partial but real".to_string()),
        reasoning_content: None,
        tool_calls: vec![],
        stop_reason: StopReason::MaxTokens,
        usage: LlmTokenUsage {
            input_tokens: 5,
            output_tokens: 128,
            ..Default::default()
        },
        provider_index: None,
    }])
    .await
    .unwrap_err();
    let incomplete = error
        .downcast_ref::<crate::agent::IncompleteResponseError>()
        .expect("truncation keeps its typed partial response");
    assert_eq!(incomplete.partial.content, "partial but real");
    assert_eq!(incomplete.partial.token_usage.input_tokens, 5);
    assert_eq!(incomplete.partial.token_usage.output_tokens, 128);
}

// --- PersistentRetryStateGuard shared-handle write-back (#1655) ---

#[test]
fn should_not_write_back_when_turn_left_retry_state_unmodified() {
    // A turn that observed no errors must not touch the shared handle at
    // all: a concurrent turn's increments landed after this guard loaded,
    // and an unconditional write-back would silently discard them.
    let handle = Arc::new(StdMutex::new(LoopRetryState::default()));
    let guard = PersistentRetryStateGuard::new(Some(handle.clone()));

    // Concurrent turn observes two rate-limits while `guard` is alive.
    {
        let mut shared = handle.lock().unwrap();
        shared.counters.rate_limited = 2;
    }

    drop(guard);
    assert_eq!(
        handle.lock().unwrap().counters.rate_limited,
        2,
        "clean turn must not clobber concurrent increments"
    );
}

#[test]
fn should_preserve_both_turns_increments_when_turns_overlap() {
    // Two turns sharing one handle, each observing different errors from
    // the same base: the merged state must reflect BOTH turns' increments,
    // so a bucket that crossed its limit cannot be rolled back to a
    // pre-exhaustion count by the later drop.
    let handle = Arc::new(StdMutex::new(LoopRetryState::default()));
    let mut turn_a = PersistentRetryStateGuard::new(Some(handle.clone()));
    let mut turn_b = PersistentRetryStateGuard::new(Some(handle.clone()));

    turn_a.counters.rate_limited += 2;
    turn_b.counters.rate_limited += 1;
    turn_b.counters.network += 3;

    drop(turn_a);
    drop(turn_b);

    let shared = handle.lock().unwrap();
    assert_eq!(shared.counters.rate_limited, 3);
    assert_eq!(shared.counters.network, 3);
}

#[test]
fn should_preserve_both_turns_increments_when_dropped_in_reverse_order() {
    // Reverse-order twin of
    // `should_preserve_both_turns_increments_when_turns_overlap` (#2221):
    // dropping turn B first exercises the same delta merge from the other
    // side — `rate_limited`, the bucket BOTH turns incremented, must still
    // accumulate 2 + 1 regardless of which drop runs the merge first.
    let handle = Arc::new(StdMutex::new(LoopRetryState::default()));
    let mut turn_a = PersistentRetryStateGuard::new(Some(handle.clone()));
    let mut turn_b = PersistentRetryStateGuard::new(Some(handle.clone()));

    turn_a.counters.rate_limited += 2;
    turn_b.counters.rate_limited += 1;
    turn_b.counters.network += 3;

    drop(turn_b);
    drop(turn_a);

    let shared = handle.lock().unwrap();
    assert_eq!(shared.counters.rate_limited, 3);
    assert_eq!(shared.counters.network, 3);
}

#[test]
fn should_write_back_exact_state_when_no_concurrent_writer() {
    // Single-agent regression: with no concurrent writer the drop must
    // reproduce today's byte-for-byte write-back, including the grace-call
    // reset of `productive_tool_calls_since_last_grace` (a non-monotonic
    // field, so a naive max-merge would corrupt it).
    let handle = Arc::new(StdMutex::new(LoopRetryState {
        productive_tool_calls_since_last_grace: 3,
        ..Default::default()
    }));
    {
        let mut guard = PersistentRetryStateGuard::new(Some(handle.clone()));
        guard.observe_budget_exhaustion(); // fires the grace call, resets the counter
        guard.counters.timeout += 1;
    }

    let shared = handle.lock().unwrap();
    assert_eq!(shared.productive_tool_calls_since_last_grace, 0);
    assert_eq!(shared.grace_calls_fired, 1);
    assert_eq!(shared.counters.timeout, 1);
}

#[test]
fn should_recover_state_from_poisoned_retry_state_mutex() {
    // A panic while holding the lock poisons the mutex; the guard must
    // still recover the inner state (with a warning) rather than panic or
    // discard it.
    let handle = Arc::new(StdMutex::new(LoopRetryState {
        grace_calls_fired: 7,
        ..Default::default()
    }));
    let poisoned = handle.clone();
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _guard = poisoned.lock().unwrap();
        panic!("simulated panic while holding the retry-state lock");
    }));
    assert!(handle.is_poisoned());

    let guard = PersistentRetryStateGuard::new(Some(handle.clone()));
    assert_eq!(guard.grace_calls_fired, 7);
    drop(guard);
    assert_eq!(
        handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .grace_calls_fired,
        7
    );

    // A DIRTY guard must also recover on drop: the write-back path takes
    // the same poisoned lock and must merge, not panic.
    {
        let mut dirty = PersistentRetryStateGuard::new(Some(handle.clone()));
        dirty.counters.timeout += 1;
    }
    let shared = handle.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(shared.grace_calls_fired, 7);
    assert_eq!(shared.counters.timeout, 1);
}

#[test]
fn should_write_nowhere_when_no_handle_attached() {
    // Legacy reset-per-turn behaviour: without a handle the guard owns a
    // fresh state and its drop touches nothing.
    let mut guard = PersistentRetryStateGuard::new(None);
    guard.counters.internal += 1;
    drop(guard); // must not panic
}
